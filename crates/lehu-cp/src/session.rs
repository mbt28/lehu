// SPDX-License-Identifier: GPL-3.0-or-later
//! The session the main loop holds: its threads, the state they share,
//! and the `lehu_session::Session` face of it. Started on a plugged
//! iPhone (`start`) or for the phones that come over Bluetooth and the
//! access point (`start_wireless`); the stack behind is the same.

use std::fmt;
use std::io;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};

use lehu_core::poll::Wake;
use lehu_core::queue::Queue;
use lehu_session::{Cmd, Event, Fault, Kind, Link, Phase, Ports, Sinks};

use crate::Config;
use crate::control::ConnHandle;
use crate::mdns;
use crate::mfi::Chip;
use crate::pairing::{MfiSigner, Store};
use crate::wireless;

/// Commands waiting for the event channel: a burst of touch frames is a
/// handful; over this the oldest goes.
const CMD_DEPTH: usize = 64;

#[derive(Debug)]
pub enum Error {
    /// The USB device is not an Apple device with a serial.
    NotAnIphone,
    /// The identity could not be loaded or made.
    Identity(io::Error),
    /// The control port could not be bound.
    Bind(io::Error),
    Spawn(io::Error),
    /// The system's access point is not up: nothing to hand the phone.
    NoAccessPoint,
    /// The Bluetooth manager is not running.
    Bluetooth(io::Error),
    /// The mDNS responder could not open its socket.
    Mdns(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotAnIphone => f.write_str("the device is not an iPhone"),
            Error::Identity(e) => write!(f, "the accessory identity: {e}"),
            Error::Bind(e) => write!(f, "the control port: {e}"),
            Error::Spawn(e) => write!(f, "cannot start a thread: {e}"),
            Error::NoAccessPoint => f.write_str("the access point is not up"),
            Error::Bluetooth(e) => write!(f, "Bluetooth: {e}"),
            Error::Mdns(e) => write!(f, "mDNS: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// Everything the threads share. The loop's ports and sinks, the
/// command queue, the phase, the chip and the identity.
pub struct Shared {
    pub config: Config,
    pub serial: String,
    /// Cleared when the session ends: every thread leaves on seeing it.
    pub running: AtomicBool,
    /// The loop asked for the end: no new attempts, the phone let go.
    pub closing: AtomicBool,
    phase: AtomicU8,
    pub ports: Ports,
    pub sinks: Mutex<Box<dyn Sinks>>,
    pub cmds: Queue<Cmd>,
    /// Wakes whoever drains the commands: the event channel's thread.
    pub cmd_wake: Wake,
    /// The coprocessor, behind the signer the handshakes use.
    pub chip: Mutex<Option<Box<dyn MfiSigner + Send>>>,
    pub store: Store,
    pub phone_name: OnceLock<String>,
    /// The phone whose iAP2 link over Bluetooth led it here (wireless).
    pub bt_peer: Mutex<String>,
    /// The loop shows the phone's video: frames are pushed; when not,
    /// they are dropped and the parameter sets kept for the return.
    pub video_active: AtomicBool,
    /// A control connection arrived at least once: the start is over.
    pub control_seen: AtomicBool,
    /// The phone opened the vehicle state: the session is CarPlay Ultra.
    pub ultra: AtomicBool,
    pub connections: Mutex<Vec<Arc<ConnHandle>>>,
}

fn phase_from(v: u8) -> Phase {
    match v {
        0 => Phase::Spawned,
        1 => Phase::Identified,
        2 => Phase::Ready,
        3 => Phase::Live,
        4 => Phase::Backgrounded,
        5 => Phase::Closing,
        _ => Phase::Closed,
    }
}

fn phase_to(p: Phase) -> u8 {
    match p {
        Phase::Spawned => 0,
        Phase::Identified => 1,
        Phase::Ready => 2,
        Phase::Live => 3,
        Phase::Backgrounded => 4,
        Phase::Closing => 5,
        Phase::Closed => 6,
    }
}

impl Shared {
    pub fn phase(&self) -> Phase {
        phase_from(self.phase.load(Ordering::Acquire))
    }

    /// Move to `p` and tell the loop; a repeat of the current phase says
    /// nothing. Closed is final.
    pub fn set_phase(&self, p: Phase) {
        let old = self.phase.swap(phase_to(p), Ordering::AcqRel);
        if old == phase_to(Phase::Closed) {
            self.phase.store(old, Ordering::Release);
            return;
        }
        if old != phase_to(p) {
            self.ports.report(Event::Phase(p));
        }
    }

    pub fn report(&self, e: Event) {
        self.ports.report(e);
    }

    pub fn sinks(&self) -> MutexGuard<'_, Box<dyn Sinks>> {
        self.sinks.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Ask every control connection to end.
    pub fn stop_connections(&self) {
        for c in self
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            c.stop.store(true, Ordering::Release);
        }
    }

    /// End the session with a fault; the loop drops it on Closed.
    pub fn fail(&self, fault: Fault) {
        if self.phase() != Phase::Closed {
            self.report(Event::Fault(fault));
        }
        self.set_phase(Phase::Closed);
        self.running.store(false, Ordering::Release);
        self.stop_connections();
        self.cmd_wake.wake();
    }
}

#[cfg(test)]
impl Shared {
    /// A session's state without a phone, for the tests of the stack.
    pub(crate) fn for_test(config: Config, sinks: Box<dyn Sinks>, ports: Ports) -> Arc<Self> {
        let store =
            Store::open(&config.state_dir.join("carplay")).expect("a store in the temp dir");
        Arc::new(Shared {
            config,
            serial: "0000TEST".into(),
            running: AtomicBool::new(true),
            closing: AtomicBool::new(false),
            phase: AtomicU8::new(phase_to(Phase::Spawned)),
            ports,
            sinks: Mutex::new(sinks),
            cmds: Queue::new(CMD_DEPTH),
            cmd_wake: Wake::new().expect("eventfd"),
            chip: Mutex::new(None),
            store,
            phone_name: OnceLock::new(),
            bt_peer: Mutex::new(String::new()),
            video_active: AtomicBool::new(true),
            control_seen: AtomicBool::new(false),
            ultra: AtomicBool::new(false),
            connections: Mutex::new(Vec::new()),
        })
    }
}

/// A running CarPlay session, wired or wireless.
pub struct Session {
    shared: Arc<Shared>,
    link: Link,
    server: Option<JoinHandle<()>>,
    trigger: Option<JoinHandle<()>>,
    /// The wireless session's announcement; dropped with it.
    mdns: Option<mdns::Responder>,
}

/// The coprocessor, when the configuration names a bus, or the first
/// `/dev/i2c-N` it answers on for "auto". Missing is a warning: the phone
/// then rejects the authentication, which the log says.
fn open_chip(config: &Config) -> Option<Chip> {
    let buses = match config.mfi_bus.as_str() {
        "" => return None,
        "auto" => i2c_buses(),
        bus => vec![bus.to_string()],
    };
    let mut last = None;
    for bus in &buses {
        match Chip::open(bus, config.mfi_addr) {
            Ok(c) => {
                log::info!("carplay: {} at {:#04x} on {bus}", c.name(), c.mfi.address);
                return Some(c);
            }
            Err(e) => last = Some(e),
        }
    }
    match last {
        Some(e) => log::warn!("carplay: no MFi coprocessor ({e}); the phone will refuse us"),
        None => log::warn!("carplay: no MFi coprocessor (no /dev/i2c-N); the phone will refuse us"),
    }
    None
}

/// CarPlay Ultra as the setting asks and the chip allows: its mutual
/// authentication is answered by an MFi 3.0 alone. The theme is read
/// here, once per session.
fn with_ultra(mut config: Config, chip: Option<&Chip>) -> Config {
    if !config.ultra {
        return config;
    }
    let major = chip.map(|c| c.protocol_major());
    if major != Some(3) {
        log::warn!(
            "carplay: ultra is on but the MFi chip is {}; classic CarPlay",
            major.map_or("missing".to_string(), |m| format!("a {m}.x"))
        );
        config.ultra = false;
        return config;
    }
    config.theme = crate::ultra::Theme::load(&config.state_dir.join("carplay"));
    log::info!(
        "carplay: ultra offered{}",
        if config.theme.is_some() {
            ", with a theme"
        } else {
            ", no theme"
        }
    );
    config
}

/// `/dev/i2c-N`, in bus order.
fn i2c_buses() -> Vec<String> {
    let mut found: Vec<u32> = std::fs::read_dir("/dev")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix("i2c-")?.parse().ok())
        .collect();
    found.sort_unstable();
    found.into_iter().map(|n| format!("/dev/i2c-{n}")).collect()
}

impl Session {
    /// Start on the plugged iPhone: the control server binds, the trigger
    /// thread begins the USB handshake. Reports through `ports`. The MFi
    /// chip missing is a warning (the phone then rejects the
    /// authentication, which the log says); everything else here is a
    /// hard error.
    pub fn start(
        config: Config,
        device: &lehu_usb::Device,
        sinks: Box<dyn Sinks>,
        ports: Ports,
    ) -> Result<Session, Error> {
        let serial = crate::usbmux::serial_of(device.name()).ok_or(Error::NotAnIphone)?;
        let (shared, listener, port) = Self::prepare(config, serial, sinks, ports)?;
        let server = Self::spawn_server(&shared, listener)?;
        let trigger = {
            let shared = Arc::clone(&shared);
            let device = *device;
            thread::Builder::new()
                .name("cp-wired".into())
                .spawn(move || crate::wired::run(shared, device, port))
                .map_err(Error::Spawn)?
        };
        log::info!(
            "carplay: wired session for {} on port {port}",
            crate::lockdown::dashed(&shared.serial)
        );
        Ok(Session {
            shared,
            link: Link::Usb,
            server: Some(server),
            trigger: Some(trigger),
            mdns: None,
        })
    }

    /// Start for the phones that come over Bluetooth: the control server
    /// binds, the iAP2 and CarPlay services are registered with
    /// bluetoothd (now, or when it comes up), the server is announced over
    /// mDNS on the access point, and every Bluetooth connection gets the
    /// bootstrap that hands the phone `ap`'s credentials. The session
    /// then waits for phones for as long as the loop keeps it; a phone's
    /// session ending (its link lost, the AP gone) closes it, and the loop
    /// starts a new one. `ap` must be up: there is nothing to hand a phone
    /// otherwise.
    pub fn start_wireless(
        config: Config,
        bt: &lehu_bt::Manager,
        ap: lehu_wifi::Params,
        sinks: Box<dyn Sinks>,
        ports: Ports,
    ) -> Result<Session, Error> {
        if !ap.valid() {
            return Err(Error::NoAccessPoint);
        }
        let (shared, listener, port) = Self::prepare(config, String::new(), sinks, ports)?;
        let ipv4 = ap.ip.parse().unwrap_or(std::net::Ipv4Addr::UNSPECIFIED);
        let pairing_id = shared.store.pairing_id.clone();
        let public_key = crate::trigger::public_key_hex(&shared.store);
        let derived_id = crate::trigger::device_id(&shared.store);
        let name = &shared.config.name;
        let hostname: String = name
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_lowercase())
            .collect();
        let announcer = mdns::Responder::start(mdns::Service {
            instance: name.clone(),
            service_type: "_airplay._tcp".into(),
            hostname: format!(
                "{}.local",
                if hostname.is_empty() {
                    "lehu"
                } else {
                    &hostname
                }
            ),
            port,
            ipv4,
            txt: vec![
                format!("deviceid={derived_id}"),
                "features=0x44540380,0x61".into(),
                "flags=0x4".into(),
                format!("srcvers={}", crate::info::SOURCE_VERSION),
                format!("pi={pairing_id}"),
                format!("pk={public_key}"),
            ],
        })
        .map_err(Error::Mdns)?;
        let [iap, iap_client, carplay] = wireless::profiles();
        let links = wireless::Links {
            iap: bt.register_rfcomm(iap).map_err(Error::Bluetooth)?,
            iap_client: bt.register_rfcomm(iap_client).map_err(Error::Bluetooth)?,
            carplay: bt.register_rfcomm(carplay).map_err(Error::Bluetooth)?,
        };
        let handoff = Arc::new(wireless::Handoff {
            device_id: if ap.bssid.is_empty() {
                derived_id
            } else {
                ap.bssid.to_ascii_uppercase()
            },
            public_key,
            adapter: bt.adapter(),
            ap,
        });
        let server = Self::spawn_server(&shared, listener)?;
        let trigger = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("cp-bt".into())
                .spawn(move || wireless::run(shared, links, handoff, port))
                .map_err(Error::Spawn)?
        };
        log::info!("carplay: wireless session on port {port}, waiting for a phone over Bluetooth");
        Ok(Session {
            shared,
            link: Link::Wifi,
            server: Some(server),
            trigger: Some(trigger),
            mdns: Some(announcer),
        })
    }

    /// What both starts share: the identity, the chip, the bound control
    /// port and the state the threads share.
    fn prepare(
        config: Config,
        serial: String,
        sinks: Box<dyn Sinks>,
        ports: Ports,
    ) -> Result<(Arc<Shared>, TcpListener, u16), Error> {
        let store = Store::open(&config.state_dir.join("carplay")).map_err(Error::Identity)?;
        let chip = open_chip(&config);
        let config = with_ultra(config, chip.as_ref());
        let listener = bind(config.port).map_err(Error::Bind)?;
        let port = listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(config.port);
        let cmd_wake = Wake::new().map_err(Error::Spawn)?;
        let shared = Arc::new(Shared {
            config,
            serial,
            running: AtomicBool::new(true),
            closing: AtomicBool::new(false),
            phase: AtomicU8::new(phase_to(Phase::Spawned)),
            ports,
            sinks: Mutex::new(sinks),
            cmds: Queue::new(CMD_DEPTH),
            cmd_wake,
            chip: Mutex::new(chip.map(|c| Box::new(c) as Box<dyn MfiSigner + Send>)),
            store,
            phone_name: OnceLock::new(),
            bt_peer: Mutex::new(String::new()),
            video_active: AtomicBool::new(true),
            control_seen: AtomicBool::new(false),
            ultra: AtomicBool::new(false),
            connections: Mutex::new(Vec::new()),
        });
        Ok((shared, listener, port))
    }

    fn spawn_server(shared: &Arc<Shared>, listener: TcpListener) -> Result<JoinHandle<()>, Error> {
        let shared = Arc::clone(shared);
        thread::Builder::new()
            .name("cp-server".into())
            .spawn(move || crate::server::run(listener, shared))
            .map_err(Error::Spawn)
    }

    /// The phone's pairing identity (its controller id) once a control
    /// connection verified: the same phone has the same id over USB and
    /// Wi-Fi, which is how the loop tells a handover from a new phone.
    pub fn controller(&self) -> Option<String> {
        self.shared
            .connections
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|c| c.verified.load(Ordering::Acquire))
            .map(|c| {
                c.controller
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            })
            .find(|c| !c.is_empty())
    }

    /// The phone's USB serial (its UDID without the dash).
    pub fn serial(&self) -> &str {
        &self.shared.serial
    }

    /// Whether `device` is this session's phone: the configuration switch
    /// re-enumerates it, so it comes back at another address, and the loop
    /// must not start a second session for it.
    pub fn same_phone(&self, device: &lehu_usb::Device) -> bool {
        crate::usbmux::serial_of(device.name()).as_deref() == Some(self.shared.serial.as_str())
    }

    fn join(&mut self) {
        if let Some(t) = self.trigger.take() {
            let _ = t.join();
        }
        if let Some(s) = self.server.take() {
            let _ = s.join();
        }
    }
}

/// A dual-stack listener on `port` (0 for any): the phone connects to our
/// link-local IPv6 over USB; a Wi-Fi phone may come over v4.
fn bind(port: u16) -> io::Result<TcpListener> {
    // A restart right after the previous instance can find the port held
    // by its dying listener; try for a few seconds.
    let mut last = None;
    for _ in 0..20 {
        match TcpListener::bind(("::", port)) {
            Ok(l) => {
                l.set_nonblocking(true)?;
                return Ok(l);
            }
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                last = Some(e);
                thread::sleep(std::time::Duration::from_millis(250));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("bind failed")))
}

impl lehu_session::Session for Session {
    fn kind(&self) -> Kind {
        Kind::CarPlay
    }

    fn link(&self) -> Link {
        self.link
    }

    fn phase(&self) -> Phase {
        self.shared.phase()
    }

    fn phone(&self) -> Option<&str> {
        self.shared.phone_name.get().map(String::as_str)
    }

    fn identity(&self) -> Option<String> {
        self.controller()
    }

    fn ultra(&self) -> bool {
        self.shared.ultra.load(Ordering::Acquire)
    }

    fn bt_address(&self) -> Option<String> {
        let peer = self
            .shared
            .bt_peer
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        (!peer.is_empty()).then(|| peer.clone())
    }

    fn command(&self, cmd: Cmd) {
        match cmd {
            Cmd::Close => {
                self.shared.closing.store(true, Ordering::Release);
                self.shared.set_phase(Phase::Closing);
                self.shared.stop_connections();
            }
            Cmd::VideoActive(on) => {
                self.shared.video_active.store(on, Ordering::Release);
                self.shared.cmds.push_drop_oldest(cmd);
            }
            other => {
                self.shared.cmds.push_drop_oldest(other);
            }
        }
        self.shared.cmd_wake.wake();
    }

    fn stop(mut self: Box<Self>) {
        let s = &self.shared;
        if s.phase() != Phase::Closed {
            s.set_phase(Phase::Closing);
        }
        s.closing.store(true, Ordering::Release);
        s.running.store(false, Ordering::Release);
        s.stop_connections();
        s.cmd_wake.wake();
        self.join();
        self.mdns = None;
        self.shared.sinks().mic(None);
        self.shared.set_phase(Phase::Closed);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::Release);
        self.shared.running.store(false, Ordering::Release);
        self.shared.stop_connections();
        self.shared.cmd_wake.wake();
        self.join();
    }
}
