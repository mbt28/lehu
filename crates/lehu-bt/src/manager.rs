// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The one Bluetooth manager: a private system-bus connection on its own
//! thread, the one pairing agent (`DisplayYesNo`, the default agent), the
//! input devices the screens list, pair, connect and forget, the adapter
//! policy, and the RFCOMM profiles the phones' bootstraps run on.
//!
//! Every public method is callable from any thread and never blocks on
//! D-Bus: commands are queued for the manager thread, which also answers
//! the agent's requests, so a phone's request is never stuck behind a
//! `Pair()` in flight. The screens poll `generation()` once a tick and copy
//! `snapshot()` only when it changed.
//!
//! The bus may not exist yet (on the image dbus-daemon runs only while
//! Bluetooth is on): the thread waits for its socket with inotify, connects
//! when it appears, and connects again after a `bt off` / `bt on`;
//! meanwhile Bluetooth shows as off. bluetoothd restarts are followed by
//! `NameOwnerChanged`.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use lehu_core::clock;
use lehu_core::poll::Wake;
use lehu_core::queue::Queue;
use lehu_core::settings::Bluetooth;

use crate::bus::{Bus, IFACE_DBUS, IFACE_LOCAL, IFACE_OM, IFACE_PROPS, Incoming, Message};
use crate::device::{self, Address, Known, is_hid_uuid, random_pin, six_digits};
use crate::ffi;
use crate::prompt::Prompt;
use crate::rfcomm::{ACCEPT_DEPTH, RfcommConnection, RfcommHandle, RfcommProfile, Role};
use crate::rules::{self, Decision, Request, Standing, Window};

/// The phone's hands-free and headset audio gateways: classic services
/// every phone has, through which a connect is always classic Bluetooth.
const UUID_HFP_AG: &str = "0000111f-0000-1000-8000-00805f9b34fb";
const UUID_HSP_AG: &str = "00001112-0000-1000-8000-00805f9b34fb";
const BLUEZ: &str = "org.bluez";
const AGENT_PATH: &str = "/lehu/agent";
/// Phones keep numeric comparison (MITM, which some insist on); keyboards
/// get passkey entry with the code shown here. KeyboardDisplay would add
/// LE cases where the head unit has to type a passkey.
const AGENT_CAPABILITY: &str = "DisplayYesNo";

const IFACE_ADAPTER: &str = "org.bluez.Adapter1";
const IFACE_DEVICE: &str = "org.bluez.Device1";
const IFACE_INPUT: &str = "org.bluez.Input1";
const IFACE_BATTERY: &str = "org.bluez.Battery1";
const IFACE_AGENT: &str = "org.bluez.Agent1";
const IFACE_AGENT_MANAGER: &str = "org.bluez.AgentManager1";
const IFACE_PROFILE: &str = "org.bluez.Profile1";
const IFACE_PROFILE_MANAGER: &str = "org.bluez.ProfileManager1";

const ERR_REJECTED: &str = "org.bluez.Error.Rejected";
const ERR_UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";

const SCAN_MS: u64 = 30_000;
const CALL_MS: i32 = 25_000;
/// Includes the user typing the code.
const PAIR_CALL_MS: i32 = 120_000;
const CONNECT_CALL_MS: i32 = 30_000;
/// A phone's service connect: answered in a few seconds when the phone is
/// there, and under the session's 8 s retry spacing, so a stuck attempt is
/// cancelled before the next one is sent.
const PHONE_CONNECT_CALL_MS: i32 = 7_000;
/// Classic keyboards usually connect HID themselves after pairing; one
/// `Connect()` after this delay covers the ones that do not.
const CONNECT_AFTER_PAIR_MS: u64 = 3000;
const STATUS_SHORT_MS: u64 = 5000;
const STATUS_LONG_MS: u64 = 8000;

/// The system bus socket. On the image it exists only while Bluetooth is
/// on; the manager waits for it and reconnects whenever it comes back.
const RUN_DIR: &str = "/var/run";
const BUS_DIR: &str = "/var/run/dbus";
const BUS_SOCKET_NAME: &str = "system_bus_socket";
const BUS_SOCKET: &str = "/var/run/dbus/system_bus_socket";
/// The socket appears at bind(), a moment before the daemon takes
/// connections: a few quick retries while it is there, then back to waiting.
const BUS_RETRY_MS: u64 = 500;
const BUS_RETRIES: u32 = 6;

/// Commands waiting for the manager thread: the screens issue one per
/// touch, so this is never near full; a full queue drops the new command
/// and says so.
const COMMAND_DEPTH: usize = 64;

/// What the manager tells its owner, on the manager thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtEvent {
    /// The snapshot changed (its generation moved).
    Changed,
    /// A phone (any device that is not an input device) connected over
    /// Bluetooth, or left. The address is all zeros when bluetoothd itself
    /// went away with a phone connected.
    PhoneConnected(Address),
    PhoneDisconnected(Address),
}

/// One row of the screens' device lists. `id` is the BlueZ object path:
/// callbacks carry it, because a row can move under a finger. Strings are
/// printable ASCII.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// "Connected  80%", "Not connected", "Keyboard  strong"...
    pub detail: String,
    pub connected: bool,
}

/// What the screens show, whole.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Bonded input devices, first-seen order.
    pub paired: Vec<Device>,
    /// Discovered, unpaired input devices, first-seen order.
    pub found: Vec<Device>,
    /// The Wireless screen's Bluetooth row: "Off", "None", "1 connected",
    /// "2 paired".
    pub summary: String,
    /// The Bluetooth screen's header suffix: "Pairing K380...",
    /// "Pairing failed - wrong code".
    pub status: String,
    pub scanning: bool,
    /// False while a wireless phone session is up.
    pub scan_allowed: bool,
    pub phone_connected: bool,
    pub prompt: Prompt,
}

pub(crate) enum Command {
    Scan(bool),
    Pair(String),
    Connect(String, bool),
    Forget(String),
    /// By address, for the device registry: no question asked (the
    /// screens asked already), nothing shown.
    RemovePhone(Address),
    ConnectPhone(Address),
    /// Hold a phone off (BlueZ's Blocked: its links drop and it cannot
    /// connect), or let it back.
    BlockPhone(Address, bool),
    PromptAccept,
    PromptCancel,
    ScreenVisible(bool),
    SessionActive(bool),
    SetName(String),
    Register(u32, RfcommProfile, Arc<Queue<RfcommConnection>>),
    Unregister(u32),
}

/// The queue the manager thread drains, and its wake.
pub(crate) struct Commands {
    queue: Queue<Command>,
    wake: Wake,
    running: AtomicBool,
}

impl Commands {
    /// Queue a command; false when the manager is not running or the queue
    /// is full.
    pub(crate) fn send(&self, cmd: Command) -> bool {
        if !self.running.load(Ordering::Acquire) {
            return false;
        }
        if !self.queue.push_discard(cmd) {
            log::warn!("bt: the command queue is full, a command was dropped");
            return false;
        }
        self.wake.wake();
        true
    }
}

/// What the manager thread publishes.
struct Published {
    generation: AtomicU32,
    snapshot: Mutex<Snapshot>,
    adapter: Mutex<Option<Address>>,
}

/// The adapter's address, readable from any thread at any time: a protocol
/// takes one at start and reads it when a phone connects, since bluetoothd
/// may come up later than the protocol.
#[derive(Clone)]
pub struct AdapterAddress {
    published: Arc<Published>,
}

impl AdapterAddress {
    pub fn get(&self) -> Option<Address> {
        *self
            .published
            .adapter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
}

/// The manager. Dropping it stops the thread.
pub struct Manager {
    commands: Arc<Commands>,
    published: Arc<Published>,
    thread: Option<JoinHandle<()>>,
    next_profile: AtomicU32,
}

impl Manager {
    /// Start the thread: it opens the bus when there is one, registers the
    /// agent and seeds the device cache. `sink` is called on the manager
    /// thread. Fails only when the thread or its wake cannot be made.
    pub fn start(
        settings: &Bluetooth,
        sink: impl FnMut(BtEvent) + Send + 'static,
    ) -> io::Result<Manager> {
        let commands = Arc::new(Commands {
            queue: Queue::new(COMMAND_DEPTH),
            wake: Wake::new()?,
            running: AtomicBool::new(true),
        });
        let published = Arc::new(Published {
            generation: AtomicU32::new(0),
            snapshot: Mutex::new(Snapshot {
                summary: "Off".into(),
                scan_allowed: true,
                ..Default::default()
            }),
            adapter: Mutex::new(None),
        });
        let name = settings.name.clone();
        let thread = {
            let commands = Arc::clone(&commands);
            let published = Arc::clone(&published);
            thread::Builder::new()
                .name("bt-manager".into())
                .spawn(move || State::new(name, commands, published, Box::new(sink)).run())?
        };
        Ok(Manager {
            commands,
            published,
            thread: Some(thread),
            next_profile: AtomicU32::new(1),
        })
    }

    /// Changes with every change of the snapshot; one atomic load a tick.
    pub fn generation(&self) -> u32 {
        self.published.generation.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> Snapshot {
        self.published
            .snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The adapter's address, once bluetoothd has told us.
    pub fn adapter_address(&self) -> Option<Address> {
        self.adapter().get()
    }

    /// A handle that reads the adapter's address later, from any thread.
    pub fn adapter(&self) -> AdapterAddress {
        AdapterAddress {
            published: Arc::clone(&self.published),
        }
    }

    /// Discovery for at most 30 s, input devices only.
    pub fn scan(&self, on: bool) {
        self.commands.send(Command::Scan(on));
    }

    pub fn pair(&self, id: &str) {
        self.commands.send(Command::Pair(id.to_string()));
    }

    pub fn connect(&self, id: &str, on: bool) {
        self.commands.send(Command::Connect(id.to_string(), on));
    }

    /// Asks a question first.
    pub fn forget(&self, id: &str) {
        self.commands.send(Command::Forget(id.to_string()));
    }

    /// Unpair the phone with this address, without a question: the
    /// device registry's Forget, whose screen asked already.
    pub fn remove_phone(&self, address: Address) {
        self.commands.send(Command::RemovePhone(address));
    }

    /// Connect the phone with this address, so a wireless session starts
    /// from the head unit's side (the registry's Connect).
    pub fn connect_phone(&self, address: Address) {
        self.commands.send(Command::ConnectPhone(address));
    }

    /// Hold the phone with this address off: BlueZ drops its links and
    /// refuses it until it is let back (`on` false). The bond stays.
    pub fn block_phone(&self, address: Address, on: bool) {
        self.commands.send(Command::BlockPhone(address, on));
    }

    pub fn prompt_accept(&self) {
        self.commands.send(Command::PromptAccept);
    }

    pub fn prompt_cancel(&self) {
        self.commands.send(Command::PromptCancel);
    }

    /// A Bluetooth screen is on screen. Leaving them stops discovery.
    pub fn screen_visible(&self, visible: bool) {
        self.commands.send(Command::ScreenVisible(visible));
    }

    /// A wireless phone session is running: discovery is refused and
    /// stopped, so inquiry never competes with it on the shared radio.
    pub fn session_active(&self, active: bool) {
        self.commands.send(Command::SessionActive(active));
    }

    /// The name the adapter shows to phones, live: the adapter's alias
    /// follows at once.
    pub fn set_name(&self, name: &str) {
        self.commands.send(Command::SetName(name.to_string()));
    }

    /// Register an RFCOMM profile; its connections arrive on the handle.
    /// Registered now if bluetoothd is up, and again whenever it comes
    /// back. While any profile is registered, phones are expected over
    /// Bluetooth: the adapter stays discoverable and their requests are
    /// accepted. Fails only when the manager is not running.
    pub fn register_rfcomm(&self, profile: RfcommProfile) -> io::Result<RfcommHandle> {
        let id = self.next_profile.fetch_add(1, Ordering::Relaxed);
        let accepted = Arc::new(Queue::new(ACCEPT_DEPTH));
        if !self
            .commands
            .send(Command::Register(id, profile, Arc::clone(&accepted)))
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the Bluetooth manager is not running",
            ));
        }
        Ok(RfcommHandle {
            id,
            accepted,
            commands: Arc::clone(&self.commands),
        })
    }

    /// Stop discovery, release the agent, leave the bus, join the thread.
    pub fn stop(mut self) {
        self.shut_down();
    }

    fn shut_down(&mut self) {
        self.commands.running.store(false, Ordering::Release);
        self.commands.wake.wake();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.shut_down();
    }
}

// ── The manager thread ────────────────────────────────────────────────────

/// What each reply is for.
enum Tag {
    NameOwner,
    ManagedObjects(String),
    RegisterAgent,
    DefaultAgent,
    StartDiscovery(String),
    StopDiscovery(String),
    Pair {
        seq: u32,
        path: String,
        name: String,
    },
    Connect(String),
    ConnectPhone(String),
    ConnectAfterPair(String),
    RemoveDevice(Option<String>),
    RegisterProfile(u32),
    /// A fire-and-forget call whose failure is worth a line.
    LogFailure(&'static str),
    /// A Discoverable set: BlueZ answers Busy to a second one while the
    /// first is still being applied, which is no failure.
    Discoverable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanState {
    Off,
    Starting,
    On,
    Stopping,
}

/// Who the open dialog belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptFor {
    None,
    /// A code the device must type (no reply owed).
    Code,
    /// A deferred RequestConfirmation (`deferred` holds it).
    Confirm,
    /// The user's own Forget, confirmed first.
    Forget,
}

struct Profile {
    id: u32,
    profile: RfcommProfile,
    accepted: Arc<Queue<RfcommConnection>>,
    path: String,
    registered: bool,
    /// RegisterProfile sent, no answer yet.
    pending: bool,
}

struct State {
    name: String,
    commands: Arc<Commands>,
    published: Arc<Published>,
    sink: Box<dyn FnMut(BtEvent) + Send>,
    last: Snapshot,

    bus: Option<Bus<Tag>>,
    bus_lost: bool,
    inotify: Option<OwnedFd>,
    run_wd: i32,
    bus_wd: i32,
    bus_try: bool,
    bus_retry_at: Option<u64>,
    bus_retries: u32,
    bus_wait_logged: bool,

    /// bluetoothd's unique bus name; empty when it is not running.
    bluez_owner: String,
    cache_ready: bool,
    agent_registered: bool,
    adapter_path: String,
    powered: bool,
    pairable: bool,
    discoverable: bool,
    /// A Discoverable set is in flight: not sent again until it answered.
    discoverable_sent: bool,
    alias: String,
    policy_applied: bool,
    devices: BTreeMap<String, Known>,
    order_seq: u32,

    // Discovery: what the user asked for (what the screens show) and where
    // BlueZ is. One Start/StopDiscovery at a time: BlueZ answers a stop that
    // overtakes a pending start (and the reverse) with InProgress and keeps
    // its own state, so a change of mind waits for the reply.
    scan_wanted: bool,
    scan_state: ScanState,
    scan_number: u32,
    scan_until: u64,
    session_active: bool,

    window: Window,
    /// Tags each `Pair()` so a reply that lost its pairing is ignored.
    pair_seq: u32,
    connect_after_pair: Option<(String, u64)>,
    /// `Connect()` in flight, for the header.
    connecting: Option<String>,

    prompt_for: PromptFor,
    prompt_path: String,
    prompt: Prompt,
    deferred: Option<Message>,

    /// Header status with an expiry.
    transient: Option<(String, u64)>,

    profiles: Vec<Profile>,
}

/// A bus call failed, for the log.
fn failed(reply: &Option<Message>) -> Option<String> {
    match reply {
        None => Some("no reply".into()),
        Some(m) if m.is_error() => Some(m.error_text()),
        Some(_) => None,
    }
}

fn error_name(reply: &Option<Message>) -> &str {
    reply.as_ref().map_or("", Message::error_name)
}

impl State {
    fn new(
        name: String,
        commands: Arc<Commands>,
        published: Arc<Published>,
        sink: Box<dyn FnMut(BtEvent) + Send>,
    ) -> Self {
        let mut s = Self {
            name,
            commands,
            published,
            sink,
            last: Snapshot::default(),
            bus: None,
            bus_lost: false,
            inotify: None,
            run_wd: -1,
            bus_wd: -1,
            bus_try: true,
            bus_retry_at: None,
            bus_retries: 0,
            bus_wait_logged: false,
            bluez_owner: String::new(),
            cache_ready: false,
            agent_registered: false,
            adapter_path: String::new(),
            powered: false,
            pairable: false,
            discoverable: false,
            discoverable_sent: false,
            alias: String::new(),
            policy_applied: false,
            devices: BTreeMap::new(),
            order_seq: 0,
            scan_wanted: false,
            scan_state: ScanState::Off,
            scan_number: 0,
            scan_until: 0,
            session_active: false,
            window: Window::new(),
            pair_seq: 0,
            connect_after_pair: None,
            connecting: None,
            prompt_for: PromptFor::None,
            prompt_path: String::new(),
            prompt: Prompt::none(),
            deferred: None,
            transient: None,
            profiles: Vec::new(),
        };
        s.last = s.snapshot();
        s
    }

    fn running(&self) -> bool {
        self.commands.running.load(Ordering::Acquire)
    }

    // ── the bus socket coming and going ───────────────────────────────────

    fn watch_bus_socket(&mut self) {
        // SAFETY: plain syscall; the result is checked.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return;
        }
        // SAFETY: a fresh descriptor we own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // The socket's directory, and its parent for the directory itself:
        // both can be missing until `bt on` runs.
        self.run_wd = add_watch(&fd, RUN_DIR);
        self.inotify = Some(fd);
        self.watch_bus_dir();
        if self.run_wd < 0 && self.bus_wd < 0 {
            log::warn!("bt: cannot watch {BUS_DIR}; a system bus started later is missed");
            self.inotify = None;
        }
    }

    fn watch_bus_dir(&mut self) {
        if let Some(fd) = &self.inotify {
            self.bus_wd = add_watch(fd, BUS_DIR);
            if self.bus_wd >= 0 {
                // The socket may be in it already.
                self.bus_try = true;
            }
        }
    }

    fn on_inotify(&mut self) {
        let Some(fd) = self.inotify.as_ref().map(AsRawFd::as_raw_fd) else {
            return;
        };
        let mut buf = [0u8; 1024];
        loop {
            // SAFETY: reading into a live buffer from our descriptor.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                return;
            }
            let bytes = &buf[..n as usize];
            let mut off = 0;
            const HEAD: usize = 16;
            while off + HEAD <= bytes.len() {
                let wd = i32::from_ne_bytes([
                    bytes[off],
                    bytes[off + 1],
                    bytes[off + 2],
                    bytes[off + 3],
                ]);
                let mask = u32::from_ne_bytes([
                    bytes[off + 4],
                    bytes[off + 5],
                    bytes[off + 6],
                    bytes[off + 7],
                ]);
                let len = u32::from_ne_bytes([
                    bytes[off + 12],
                    bytes[off + 13],
                    bytes[off + 14],
                    bytes[off + 15],
                ]) as usize;
                let end = off + HEAD + len;
                if end > bytes.len() {
                    break;
                }
                let name = &bytes[off + HEAD..end];
                let name = name.split(|b| *b == 0).next().unwrap_or(&[]);
                off = end;
                if mask & libc::IN_IGNORED != 0 {
                    if wd == self.bus_wd {
                        // BUS_DIR removed; RUN_DIR's watch sees it return.
                        self.bus_wd = -1;
                    }
                    continue;
                }
                if wd == self.run_wd && name == b"dbus" {
                    self.watch_bus_dir();
                } else if wd == self.bus_wd && name == BUS_SOCKET_NAME.as_bytes() {
                    self.bus_try = true;
                    self.bus_retries = 0;
                }
            }
        }
    }

    fn connect_bus(&mut self) {
        self.bus_try = false;
        self.bus_retry_at = None;
        let bus = match Bus::<Tag>::connect_system() {
            Ok(b) => b,
            Err(why) => {
                if std::path::Path::new(BUS_SOCKET).exists() && self.bus_retries < BUS_RETRIES {
                    self.bus_retries += 1;
                    self.bus_retry_at = Some(clock::now_ms() + BUS_RETRY_MS);
                }
                if !self.bus_wait_logged {
                    log::info!("bt: no system bus yet ({why}); waiting for it");
                }
                self.bus_wait_logged = true;
                return;
            }
        };
        if self.bus_wait_logged {
            log::info!("bt: the system bus is up");
        }
        self.bus_wait_logged = false;
        self.bus_retries = 0;
        let mut bus = bus;
        bus.register_path(AGENT_PATH);
        for p in &self.profiles {
            bus.register_path(&p.path);
        }
        bus.add_match(
            "type='signal',sender='org.bluez',path='/',interface='org.freedesktop.DBus.ObjectManager'",
        );
        bus.add_match(
            "type='signal',sender='org.bluez',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged',path_namespace='/org/bluez'",
        );
        bus.add_match(
            "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='org.bluez'",
        );
        self.bus = Some(bus);
        // Is bluetoothd running now? Later starts and stops arrive as
        // NameOwnerChanged.
        if let Some(mut m) = self.message(
            IFACE_DBUS,
            "/org/freedesktop/DBus",
            IFACE_DBUS,
            "GetNameOwner",
        ) {
            m.append().str(BLUEZ);
            self.call(m, CALL_MS, Tag::NameOwner);
        }
    }

    /// Leave the bus, politely when it is still there (a lost bus has
    /// nobody to tell), and forget everything BlueZ said.
    fn disconnect_bus(&mut self) {
        let Some(bus) = self.bus.take() else {
            return;
        };
        if !self.bus_lost && bus.connected() {
            // Closing the connection would end our discovery as well.
            if self.scan_state != ScanState::Off
                && !self.adapter_path.is_empty()
                && let Some(m) = Message::method_call(
                    bus.lib(),
                    BLUEZ,
                    &self.adapter_path,
                    IFACE_ADAPTER,
                    "StopDiscovery",
                )
            {
                m.set_no_reply();
                bus.send(m);
            }
            if self.agent_registered
                && let Some(mut m) = Message::method_call(
                    bus.lib(),
                    BLUEZ,
                    "/org/bluez",
                    IFACE_AGENT_MANAGER,
                    "UnregisterAgent",
                )
            {
                m.append().path(AGENT_PATH);
                m.set_no_reply();
                bus.send(m);
            }
            for p in &self.profiles {
                if p.registered
                    && let Some(mut m) = Message::method_call(
                        bus.lib(),
                        BLUEZ,
                        "/org/bluez",
                        IFACE_PROFILE_MANAGER,
                        "UnregisterProfile",
                    )
                {
                    m.append().path(&p.path);
                    m.set_no_reply();
                    bus.send(m);
                }
            }
            if let Some(d) = self.deferred.take()
                && let Some(r) = Message::error_to(&d, ERR_REJECTED, "Leaving")
            {
                bus.send(r);
            }
            bus.flush();
        }
        drop(bus);
        self.bus_lost = false;
        self.reset_bluez();
    }

    /// Forget everything bluetoothd told us (it is gone, or the bus is).
    fn reset_bluez(&mut self) {
        self.bluez_owner.clear();
        self.cache_ready = false;
        self.agent_registered = false;
        self.adapter_path.clear();
        self.policy_applied = false;
        self.devices.clear();
        self.scan_wanted = false;
        self.scan_state = ScanState::Off;
        self.window.abandon();
        self.connect_after_pair = None;
        self.connecting = None;
        self.close_prompt(false);
        for p in &mut self.profiles {
            p.registered = false;
        }
        *self
            .published
            .adapter
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    // ── calls ─────────────────────────────────────────────────────────────

    fn message(&self, dest: &str, path: &str, iface: &str, member: &str) -> Option<Message> {
        let bus = self.bus.as_ref()?;
        Message::method_call(bus.lib(), dest, path, iface, member)
    }

    fn bluez(&self, path: &str, iface: &str, member: &str) -> Option<Message> {
        self.message(BLUEZ, path, iface, member)
    }

    fn call(&self, msg: Message, timeout_ms: i32, tag: Tag) {
        if let Some(bus) = &self.bus {
            bus.call(msg, timeout_ms, tag);
        }
    }

    fn send(&self, msg: Option<Message>) {
        if let (Some(bus), Some(m)) = (&self.bus, msg) {
            bus.send(m);
        }
    }

    fn reply_empty(&self, to: &Message) {
        self.send(Message::reply_to(to));
    }

    fn reply_error(&self, to: &Message, name: &str, text: &str) {
        self.send(Message::error_to(to, name, text));
    }

    fn reply_string(&self, to: &Message, value: &str) {
        if let Some(mut r) = Message::reply_to(to) {
            r.append().str(value);
            self.send(Some(r));
        }
    }

    fn set_bool(&self, path: &str, iface: &str, name: &str, value: bool, what: &'static str) {
        if let Some(mut m) = self.bluez(path, IFACE_PROPS, "Set") {
            let mut a = m.append();
            a.str(iface).str(name);
            let mut v = a.open(ffi::TYPE_VARIANT, Some("b"));
            v.bool(value);
            a.close(v);
            self.call(m, CALL_MS, Tag::LogFailure(what));
        }
    }

    fn set_u32(&self, path: &str, iface: &str, name: &str, value: u32, what: &'static str) {
        if let Some(mut m) = self.bluez(path, IFACE_PROPS, "Set") {
            let mut a = m.append();
            a.str(iface).str(name);
            let mut v = a.open(ffi::TYPE_VARIANT, Some("u"));
            v.u32(value);
            a.close(v);
            self.call(m, CALL_MS, Tag::LogFailure(what));
        }
    }

    fn set_str(&self, path: &str, iface: &str, name: &str, value: &str, what: &'static str) {
        if let Some(mut m) = self.bluez(path, IFACE_PROPS, "Set") {
            let mut a = m.append();
            a.str(iface).str(name);
            let mut v = a.open(ffi::TYPE_VARIANT, Some("s"));
            v.str(value);
            a.close(v);
            self.call(m, CALL_MS, Tag::LogFailure(what));
        }
    }

    fn device_call(&self, path: &str, member: &str, timeout_ms: i32, tag: Tag) {
        if let Some(m) = self.bluez(path, IFACE_DEVICE, member) {
            self.call(m, timeout_ms, tag);
        }
    }

    fn adapter_call(&self, member: &str, device: Option<&str>, tag: Tag) {
        if let Some(mut m) = self.bluez(&self.adapter_path, IFACE_ADAPTER, member) {
            if let Some(d) = device {
                m.append().path(d);
            }
            self.call(m, CALL_MS, tag);
        }
    }

    // ── the device cache ──────────────────────────────────────────────────

    fn device(&mut self, path: &str) -> &mut Known {
        if !self.devices.contains_key(path) {
            self.order_seq += 1;
            let order = self.order_seq;
            self.devices.insert(
                path.to_string(),
                Known {
                    order,
                    ..Default::default()
                },
            );
        }
        self.devices.get_mut(path).expect("inserted above")
    }

    fn name_of(&self, path: &str) -> String {
        self.devices
            .get(path)
            .map_or_else(|| "device".to_string(), Known::display_name)
    }

    fn is_hid(&self, path: &str) -> bool {
        self.devices.get(path).is_some_and(Known::is_hid)
    }

    /// A phone, any device that is not an input device, is connected.
    fn phone_connected(&self) -> bool {
        self.devices.values().any(|d| d.connected && !d.is_hid())
    }

    /// Phones pair over Bluetooth in this mode: a protocol registered an
    /// RFCOMM profile for them.
    fn phone_pairing(&self) -> bool {
        !self.profiles.is_empty()
    }

    /// In a wireless mode a phone connected over Bluetooth is its session,
    /// or the handoff before it, on the radio that discovery would share.
    fn scan_blocked(&self) -> bool {
        self.session_active || (self.phone_pairing() && self.phone_connected())
    }

    fn ready(&self) -> bool {
        self.cache_ready && !self.adapter_path.is_empty()
    }

    fn apply_device(&mut self, path: &str, props: &mut crate::bus::Iter<'_>) {
        let mut key_stored = false;
        let mut connected_edge: Option<bool> = None;
        let scan_wanted = self.scan_wanted;
        let scan_number = self.scan_number;
        {
            let d = self.device(path);
            props.for_each_variant(|key, v| match key {
                "Address" => {
                    if let Some(s) = v.str() {
                        d.address = s.to_string();
                    }
                }
                "Alias" => {
                    if let Some(s) = v.str() {
                        d.alias = s.to_string();
                    }
                }
                "Icon" => {
                    if let Some(s) = v.str() {
                        d.icon = s.to_string();
                    }
                }
                "Class" => d.class = v.u32().or(d.class),
                "Appearance" => d.appearance = v.u16().or(d.appearance),
                "RSSI" => {
                    if let Some(r) = v.i16() {
                        d.rssi = Some(r);
                        if scan_wanted {
                            d.seen_scan = scan_number;
                        }
                    }
                }
                "UUIDs" => {
                    let mut hid = false;
                    let mut list = Vec::new();
                    v.for_each_string(|u| {
                        hid |= is_hid_uuid(u);
                        list.push(u.to_ascii_lowercase());
                    });
                    d.hid_uuid = hid;
                    d.uuids = list;
                }
                "Paired" => {
                    if let Some(b) = v.bool() {
                        key_stored |= b && !d.paired;
                        d.paired = b;
                    }
                }
                "Bonded" => {
                    // Bonded comes with the stored key; Paired (and the
                    // Pair() reply) only after the service search after it.
                    if let Some(b) = v.bool() {
                        key_stored |= b && !d.bonded;
                        d.bonded = b;
                    }
                }
                "Trusted" => {
                    if let Some(b) = v.bool() {
                        d.trusted = b;
                    }
                }
                "Connected" => {
                    if let Some(b) = v.bool() {
                        if b != d.connected {
                            connected_edge = Some(b);
                        }
                        d.connected = b;
                    }
                }
                _ => {}
            });
        }
        if key_stored {
            self.on_key_stored(path);
        }
        if let Some(on) = connected_edge
            && !self.is_hid(path)
        {
            let address = Address::parse(path).unwrap_or_default();
            if on && self.scan_wanted && self.phone_pairing() {
                log::info!("bt: a phone connected; the search stops");
                self.stop_scan();
            }
            (self.sink)(if on {
                BtEvent::PhoneConnected(address)
            } else {
                BtEvent::PhoneDisconnected(address)
            });
        }
    }

    fn invalidate_device(&mut self, path: &str, names: &mut crate::bus::Iter<'_>) {
        if let Some(d) = self.devices.get_mut(path) {
            names.for_each_string(|n| match n {
                "RSSI" => d.rssi = None,
                "Icon" => d.icon.clear(),
                "Class" => d.class = None,
                "Appearance" => d.appearance = None,
                _ => {}
            });
        }
    }

    fn apply_adapter(&mut self, path: &str, props: &mut crate::bus::Iter<'_>) {
        if self.adapter_path.is_empty() || path == "/org/bluez/hci0" {
            if self.adapter_path != path {
                self.policy_applied = false;
            }
            self.adapter_path = path.to_string();
        }
        if path != self.adapter_path {
            return;
        }
        let mut address = None;
        props.for_each_variant(|key, v| match key {
            "Powered" => self.powered = v.bool().unwrap_or(self.powered),
            "Pairable" => self.pairable = v.bool().unwrap_or(self.pairable),
            "Discoverable" => self.discoverable = v.bool().unwrap_or(self.discoverable),
            "Alias" => {
                if let Some(s) = v.str() {
                    self.alias = s.to_string();
                }
            }
            "Address" => address = v.str().and_then(Address::parse),
            _ => {}
        });
        if let Some(a) = address {
            *self
                .published
                .adapter
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(a);
        }
    }

    fn apply_battery(&mut self, path: &str, props: &mut crate::bus::Iter<'_>) {
        let d = self.device(path);
        props.for_each_variant(|key, v| {
            if key == "Percentage"
                && let Some(p) = v.byte()
            {
                d.battery = Some(p);
            }
        });
    }

    fn interface_added(&mut self, path: &str, iface: &str, props: &mut crate::bus::Iter<'_>) {
        match iface {
            IFACE_DEVICE => self.apply_device(path, props),
            IFACE_INPUT => self.device(path).has_input = true,
            IFACE_BATTERY => self.apply_battery(path, props),
            IFACE_ADAPTER => self.apply_adapter(path, props),
            _ => {}
        }
    }

    fn interface_removed(&mut self, path: &str, iface: &str) {
        match iface {
            IFACE_DEVICE => {
                let was_connected_phone = self
                    .devices
                    .get(path)
                    .is_some_and(|d| d.connected && !d.is_hid());
                self.devices.remove(path);
                if was_connected_phone {
                    (self.sink)(BtEvent::PhoneDisconnected(
                        Address::parse(path).unwrap_or_default(),
                    ));
                }
                if self
                    .connect_after_pair
                    .as_ref()
                    .is_some_and(|(p, _)| p == path)
                {
                    self.connect_after_pair = None;
                }
                if self.prompt_path == path && self.prompt_for != PromptFor::None {
                    self.close_prompt(false);
                }
                if self.window.target() == Some(path) {
                    // Removing a device cancels its service search without
                    // answering the Pair() waiting on it.
                    if self.window.pair_pending() {
                        let text = if self.window.user_cancelled() {
                            "Pairing cancelled"
                        } else {
                            "Pairing failed"
                        };
                        self.set_status(text, STATUS_LONG_MS);
                    }
                    self.window.abandon();
                }
            }
            IFACE_INPUT => {
                if let Some(d) = self.devices.get_mut(path) {
                    d.has_input = false;
                }
            }
            IFACE_BATTERY => {
                if let Some(d) = self.devices.get_mut(path) {
                    d.battery = None;
                }
            }
            IFACE_ADAPTER if path == self.adapter_path => {
                log::warn!("bt: adapter {path} gone");
                self.adapter_path.clear();
                self.scan_wanted = false;
                self.scan_state = ScanState::Off;
                *self
                    .published
                    .adapter
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
            }
            _ => {}
        }
    }

    // ── adapter policy, the agent, the profiles ───────────────────────────

    fn apply_policy(&mut self) {
        if self.adapter_path.is_empty() || self.policy_applied {
            return;
        }
        self.policy_applied = true;
        let path = self.adapter_path.clone();
        // Never power down or reset: a connected keyboard survives
        // everything.
        if !self.powered {
            self.set_bool(&path, IFACE_ADAPTER, "Powered", true, "Powered=true");
        }
        // Without Pairable even our own Pair() would not store a key.
        if !self.pairable {
            self.set_bool(&path, IFACE_ADAPTER, "Pairable", true, "Pairable=true");
        }
        if !self.name.is_empty() && self.alias != self.name {
            let name = self.name.clone();
            self.set_str(&path, IFACE_ADAPTER, "Alias", &name, "Alias");
        }
        if self.phone_pairing() {
            // A phone needs to find the head unit to pair; for as long as
            // the mode lasts.
            self.set_u32(
                &path,
                IFACE_ADAPTER,
                "PairableTimeout",
                0,
                "PairableTimeout=0",
            );
            self.set_u32(
                &path,
                IFACE_ADAPTER,
                "DiscoverableTimeout",
                0,
                "DiscoverableTimeout=0",
            );
            if !self.discoverable && !self.discoverable_sent {
                self.set_discoverable(&path, true);
            }
        } else if self.discoverable && !self.discoverable_sent {
            // Wired modes: nobody needs to find the head unit; bonded
            // keyboards still reconnect (BlueZ keeps them on the page-scan
            // accept list).
            self.set_discoverable(&path, false);
        }
    }

    fn set_discoverable(&mut self, path: &str, on: bool) {
        if let Some(mut m) = self.bluez(path, IFACE_PROPS, "Set") {
            let mut a = m.append();
            a.str(IFACE_ADAPTER).str("Discoverable");
            let mut v = a.open(ffi::TYPE_VARIANT, Some("b"));
            v.bool(on);
            a.close(v);
            self.call(m, CALL_MS, Tag::Discoverable);
            self.discoverable_sent = true;
        }
    }

    fn register_agent(&mut self) {
        if let Some(mut m) = self.bluez("/org/bluez", IFACE_AGENT_MANAGER, "RegisterAgent") {
            m.append().path(AGENT_PATH).str(AGENT_CAPABILITY);
            self.call(m, CALL_MS, Tag::RegisterAgent);
        }
    }

    fn register_profiles(&mut self) {
        if !self.ready() {
            return;
        }
        // Once each: a registration whose answer is still out is not sent
        // again (the policy re-applies after every new profile).
        let todo: Vec<(u32, String, RfcommProfile)> = self
            .profiles
            .iter()
            .filter(|p| !p.registered && !p.pending)
            .map(|p| (p.id, p.path.clone(), p.profile.clone()))
            .collect();
        for (id, path, profile) in todo {
            if let Some(p) = self.profiles.iter_mut().find(|p| p.id == id) {
                p.pending = true;
            }
            let Some(mut m) = self.bluez("/org/bluez", IFACE_PROFILE_MANAGER, "RegisterProfile")
            else {
                continue;
            };
            {
                let mut a = m.append();
                a.path(&path).str(&profile.uuid);
                let mut dict = a.open(ffi::TYPE_ARRAY, Some("{sv}"));
                let server = profile.role == Role::Server;
                dict.variant_str("Role", if server { "server" } else { "client" });
                dict.variant_str("Name", &profile.name);
                if server && profile.channel != 0 {
                    dict.variant_u16("Channel", u16::from(profile.channel));
                }
                if let Some(r) = &profile.record {
                    dict.variant_str("ServiceRecord", r);
                }
                dict.variant_bool("RequireAuthentication", false);
                dict.variant_bool("RequireAuthorization", false);
                if !server {
                    dict.variant_bool("AutoConnect", true);
                }
                a.close(dict);
            }
            self.call(m, CALL_MS, Tag::RegisterProfile(id));
        }
    }

    fn on_managed_objects(&mut self, reply: Option<Message>) {
        let reply = match reply {
            Some(r) if !r.is_error() => r,
            other => {
                log::error!(
                    "bt: GetManagedObjects failed > {}",
                    failed(&other).unwrap_or_default()
                );
                return;
            }
        };
        // A fresh view: keep only list order and scan sightings of known
        // paths.
        let old = std::mem::take(&mut self.devices);
        if let Some(mut objects) = reply.args() {
            objects.for_each_entry(|path, ifaces| {
                ifaces.for_each_entry(|iface, props| {
                    self.interface_added(path, iface, props);
                });
            });
        }
        for (path, d) in &mut self.devices {
            if let Some(prev) = old.get(path) {
                d.order = prev.order;
                d.seen_scan = prev.seen_scan;
            }
        }
        self.cache_ready = true;
        if self.adapter_path.is_empty() {
            log::warn!("bt: no Bluetooth adapter");
        } else {
            log::info!(
                "bt: adapter {}, {} known device(s)",
                self.adapter_path,
                self.devices.len()
            );
        }
        self.apply_policy();
        if !self.agent_registered {
            self.register_agent();
        }
        self.register_profiles();
    }

    fn bluez_up(&mut self, owner: &str) {
        if owner == self.bluez_owner {
            return;
        }
        self.bluez_down();
        self.bluez_owner = owner.to_string();
        log::info!("bt: bluetoothd is up ({owner})");
        if let Some(m) = self.bluez("/", IFACE_OM, "GetManagedObjects") {
            self.call(m, CALL_MS, Tag::ManagedObjects(owner.to_string()));
        }
    }

    fn bluez_down(&mut self) {
        if self.bluez_owner.is_empty() {
            return;
        }
        log::warn!("bt: bluetoothd went away");
        let had_phone = self.phone_connected();
        self.reset_bluez();
        if had_phone {
            (self.sink)(BtEvent::PhoneDisconnected(Address::default()));
        }
    }

    // ── the dialog ────────────────────────────────────────────────────────

    fn open_prompt(&mut self, kind: PromptFor, path: &str, p: Prompt) {
        // A newer request replaces an older dialog.
        self.close_prompt(true);
        self.prompt_for = kind;
        self.prompt_path = path.to_string();
        self.prompt = p;
    }

    /// `reject_deferred`: answer a still-pending RequestConfirmation with
    /// "no" (false when BlueZ already gave up on it, e.g. after Cancel()).
    fn close_prompt(&mut self, reject_deferred: bool) {
        if let Some(d) = self.deferred.take()
            && reject_deferred
        {
            self.reply_error(&d, ERR_REJECTED, "Replaced");
        }
        self.prompt_for = PromptFor::None;
        self.prompt_path.clear();
        self.prompt = Prompt::none();
    }

    fn show_code(&mut self, path: &str, code: &str, entered: u16) {
        let p = Prompt::code(&self.name_of(path), code, entered);
        if self.prompt_for == PromptFor::Code && self.prompt_path == path {
            // A progress update: the same dialog.
            self.prompt = p;
        } else {
            self.open_prompt(PromptFor::Code, path, p);
        }
    }

    /// The key is stored: from here on CancelPairing would not cancel but
    /// silently unpair, so the code dialog (and its Cancel) goes away.
    fn on_key_stored(&mut self, path: &str) {
        if self.prompt_for == PromptFor::Code && self.prompt_path == path {
            self.close_prompt(false);
        }
    }

    fn set_status(&mut self, text: &str, ms: u64) {
        self.transient = Some((text.to_string(), clock::now_ms() + ms));
    }

    // ── the agent ─────────────────────────────────────────────────────────

    fn standing(&self, path: &str) -> Standing {
        Standing {
            target: self.window.is_target(path, clock::now_ms()),
            hid: self.is_hid(path),
            phone_pairing: self.phone_pairing(),
        }
    }

    fn reject(&self, msg: &Message, dev: &str, what: &str) {
        if self.is_hid(dev) {
            log::warn!("bt: rejected {what} from input device {dev} (not started here)");
            self.reply_error(msg, ERR_REJECTED, "Pair it from the head unit");
        } else {
            log::info!("bt: refused {what} from {dev} (no phone uses Bluetooth in this mode)");
            self.reply_error(msg, ERR_REJECTED, "Not in this mode");
        }
    }

    fn on_agent(&mut self, msg: Message) {
        if msg.interface() != IFACE_AGENT {
            self.reply_error(&msg, ERR_UNKNOWN_METHOD, "Not an agent method");
            return;
        }
        if self.bluez_owner.is_empty() || msg.sender() != self.bluez_owner {
            self.reply_error(&msg, ERR_REJECTED, "Not from bluetoothd");
            return;
        }
        let member = msg.member().to_string();
        let mut args = msg.args();
        let dev = args.as_mut().and_then(|a| a.path()).map(str::to_string);
        match member.as_str() {
            "Release" => {
                log::warn!("bt: agent released by bluetoothd");
                self.agent_registered = false;
                self.close_prompt(false);
                self.reply_empty(&msg);
            }
            "Cancel" => {
                log::info!("bt: agent request cancelled");
                if matches!(self.prompt_for, PromptFor::Confirm | PromptFor::Code) {
                    self.close_prompt(false);
                }
                self.reply_empty(&msg);
            }
            "RequestConfirmation" => {
                let passkey = args.as_mut().and_then(|a| {
                    a.next();
                    a.u32()
                });
                let (Some(dev), Some(passkey)) = (dev, passkey) else {
                    self.reply_error(&msg, ERR_REJECTED, "Bad arguments");
                    return;
                };
                match rules::decide(Request::Confirmation, self.standing(&dev)) {
                    Decision::Prompt => {
                        // A device with a display: the user compares the codes.
                        let p = Prompt::confirm(&self.name_of(&dev), passkey);
                        self.open_prompt(PromptFor::Confirm, &dev, p);
                        self.deferred = Some(msg);
                    }
                    Decision::Accept => self.reply_empty(&msg),
                    _ => self.reject(&msg, &dev, "pairing"),
                }
            }
            "RequestAuthorization" => {
                let Some(dev) = dev else {
                    self.reply_error(&msg, ERR_REJECTED, "Bad arguments");
                    return;
                };
                match rules::decide(Request::Authorization, self.standing(&dev)) {
                    Decision::Accept => self.reply_empty(&msg),
                    _ => self.reject(&msg, &dev, "just-works pairing"),
                }
            }
            "AuthorizeService" => {
                let uuid = args.as_mut().and_then(|a| {
                    a.next();
                    a.str()
                });
                let (Some(dev), Some(uuid)) = (dev, uuid) else {
                    self.reply_error(&msg, ERR_REJECTED, "Bad arguments");
                    return;
                };
                let hid = is_hid_uuid(uuid);
                match rules::decide(Request::AuthorizeService { hid }, self.standing(&dev)) {
                    Decision::Accept => self.reply_empty(&msg),
                    _ if hid => {
                        // Trusted devices never get here: an unknown keyboard.
                        log::warn!("bt: rejected HID service from untrusted device {dev}");
                        self.reply_error(&msg, ERR_REJECTED, "Not trusted");
                    }
                    _ => self.reject(&msg, &dev, uuid),
                }
            }
            "DisplayPasskey" => {
                let code = args.as_mut().and_then(|a| {
                    a.next();
                    let passkey = a.u32()?;
                    a.next();
                    Some((passkey, a.u16().unwrap_or(0)))
                });
                if let (Some(dev), Some((passkey, entered))) = (dev, code) {
                    match rules::decide(Request::DisplayPasskey, self.standing(&dev)) {
                        Decision::Prompt => self.show_code(&dev, &six_digits(passkey), entered),
                        Decision::Disconnect => {
                            // Cannot be refused by a reply: end it by
                            // dropping the link.
                            log::warn!("bt: input device {dev} tried to pair; disconnecting");
                            self.device_call(
                                &dev,
                                "Disconnect",
                                CALL_MS,
                                Tag::LogFailure("Disconnect"),
                            );
                        }
                        _ => {}
                    }
                }
                self.reply_empty(&msg);
            }
            "DisplayPinCode" => {
                // Must be answered at once: bluetoothd sends the PIN either
                // way.
                let pin = args.as_mut().and_then(|a| {
                    a.next();
                    a.str()
                });
                if let (Some(dev), Some(pin)) = (dev, pin)
                    && rules::decide(Request::DisplayPinCode, self.standing(&dev))
                        == Decision::Prompt
                {
                    let pin = pin.to_string();
                    self.show_code(&dev, &pin, 0);
                }
                self.reply_empty(&msg);
            }
            "RequestPinCode" => {
                // Legacy (pre-2.1) devices. A keyboard types the PIN we
                // show; a remote or game pad uses the conventional fixed one.
                let Some(dev) = dev else {
                    self.reply_error(&msg, ERR_REJECTED, "Bad arguments");
                    return;
                };
                match rules::decide(Request::RequestPinCode, self.standing(&dev)) {
                    Decision::Pin => {
                        let keyboard = self
                            .devices
                            .get(&dev)
                            .is_some_and(|d| d.kind() == device::Kind::Keyboard);
                        if keyboard {
                            let pin = random_pin();
                            self.show_code(&dev, &pin, 0);
                            self.reply_string(&msg, &pin);
                        } else {
                            self.reply_string(&msg, "0000");
                        }
                    }
                    _ => {
                        log::warn!("bt: rejected PIN pairing from {dev} (not started here)");
                        self.reply_error(&msg, ERR_REJECTED, "Pair it from the head unit");
                    }
                }
            }
            "RequestPasskey" => {
                // Unreachable with DisplayYesNo; never type on the head unit.
                self.reply_error(&msg, ERR_REJECTED, "Not supported");
            }
            _ => self.reply_error(&msg, ERR_UNKNOWN_METHOD, "Unknown agent method"),
        }
    }

    // ── the profiles ──────────────────────────────────────────────────────

    fn on_profile(&mut self, msg: Message) {
        let path = msg.path().to_string();
        let Some(i) = self.profiles.iter().position(|p| p.path == path) else {
            self.reply_error(&msg, ERR_UNKNOWN_METHOD, "No such profile");
            return;
        };
        if msg.interface() != IFACE_PROFILE {
            self.reply_error(&msg, ERR_UNKNOWN_METHOD, "Not a profile method");
            return;
        }
        match msg.member() {
            "NewConnection" => {
                let mut args = msg.args();
                let dev = args.as_mut().and_then(|a| a.path()).map(str::to_string);
                let fd = args.as_mut().and_then(|a| {
                    a.next();
                    a.fd()
                });
                match (dev, fd) {
                    (Some(dev), Some(fd)) => {
                        let peer = Address::parse(&dev).unwrap_or_default();
                        log::info!(
                            "bt: {} connection from {peer}",
                            self.profiles[i].profile.name
                        );
                        // SAFETY: a descriptor libdbus duplicated for us.
                        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                        let conn = RfcommConnection { fd, peer };
                        // A full queue closes the newcomer: the protocol has
                        // two unread connections already.
                        if !self.profiles[i].accepted.push_discard(conn) {
                            log::warn!("bt: a third unread connection on {path}, closed");
                        }
                        self.reply_empty(&msg);
                    }
                    _ => self.reply_error(&msg, ERR_REJECTED, "Bad arguments"),
                }
            }
            "RequestDisconnection" => self.reply_empty(&msg),
            "Release" => {
                self.profiles[i].registered = false;
                self.reply_empty(&msg);
            }
            _ => self.reply_error(&msg, ERR_UNKNOWN_METHOD, "Unknown profile method"),
        }
    }

    fn do_register(
        &mut self,
        id: u32,
        profile: RfcommProfile,
        accepted: Arc<Queue<RfcommConnection>>,
    ) {
        let path = format!("/lehu/rfcomm/{id}");
        if let Some(bus) = &mut self.bus {
            bus.register_path(&path);
        }
        self.profiles.push(Profile {
            id,
            profile,
            accepted,
            path,
            registered: false,
            pending: false,
        });
        // The mode changed: the adapter policy follows.
        self.policy_applied = false;
        self.apply_policy();
        self.register_profiles();
    }

    fn unregister_path(&self, path: &str) {
        if let Some(mut m) = self.bluez("/org/bluez", IFACE_PROFILE_MANAGER, "UnregisterProfile") {
            m.append().path(path);
            self.call(m, CALL_MS, Tag::LogFailure("UnregisterProfile"));
        }
    }

    fn do_unregister(&mut self, id: u32) {
        let Some(i) = self.profiles.iter().position(|p| p.id == id) else {
            return;
        };
        let p = self.profiles.remove(i);
        // A registration still unanswered is unregistered when its answer
        // comes (see the RegisterProfile reply).
        if p.registered {
            self.unregister_path(&p.path);
        }
        if let Some(bus) = &mut self.bus {
            bus.unregister_path(&p.path);
        }
        p.accepted.close();
        self.policy_applied = false;
        self.apply_policy();
    }

    // ── signals ───────────────────────────────────────────────────────────

    fn on_signal(&mut self, msg: Message) {
        if msg.is_signal(IFACE_LOCAL, "Disconnected") {
            log::warn!("bt: system bus gone; waiting for it to come back");
            self.bus_lost = true;
            self.bluez_down();
            return;
        }
        if msg.is_signal(IFACE_DBUS, "NameOwnerChanged") {
            let Some(mut a) = msg.args() else {
                return;
            };
            let name = a.str().unwrap_or("");
            if name != BLUEZ {
                return;
            }
            a.next();
            a.next();
            let new_owner = a.str().unwrap_or("").to_string();
            if new_owner.is_empty() {
                self.bluez_down();
            } else {
                self.bluez_up(&new_owner);
            }
            return;
        }
        if self.bluez_owner.is_empty() || msg.sender() != self.bluez_owner {
            return;
        }
        if msg.is_signal(IFACE_OM, "InterfacesAdded") {
            let Some(mut a) = msg.args() else {
                return;
            };
            let Some(path) = a.path() else {
                return;
            };
            a.next();
            let had_adapter = !self.adapter_path.is_empty();
            a.for_each_entry(|iface, props| self.interface_added(path, iface, props));
            if !had_adapter && !self.adapter_path.is_empty() && self.cache_ready {
                log::info!("bt: adapter {} appeared", self.adapter_path);
                self.apply_policy();
                self.register_profiles();
            }
        } else if msg.is_signal(IFACE_OM, "InterfacesRemoved") {
            let Some(mut a) = msg.args() else {
                return;
            };
            let Some(path) = a.path() else {
                return;
            };
            a.next();
            let mut gone = Vec::new();
            a.for_each_string(|iface| gone.push(iface.to_string()));
            for iface in gone {
                self.interface_removed(path, &iface);
            }
        } else if msg.is_signal(IFACE_PROPS, "PropertiesChanged") {
            let path = msg.path().to_string();
            let Some(mut a) = msg.args() else {
                return;
            };
            let Some(iface) = a.str() else {
                return;
            };
            a.next();
            match iface {
                IFACE_DEVICE => {
                    // Not announced yet (InterfacesAdded comes first).
                    if !self.devices.contains_key(&path) {
                        return;
                    }
                    self.apply_device(&path, &mut a);
                    a.next();
                    self.invalidate_device(&path, &mut a);
                }
                IFACE_BATTERY => {
                    if self.devices.contains_key(&path) {
                        self.apply_battery(&path, &mut a);
                    }
                }
                IFACE_ADAPTER => self.apply_adapter(&path, &mut a),
                _ => {}
            }
        }
    }

    // ── replies ───────────────────────────────────────────────────────────

    fn on_reply(&mut self, tag: Tag, reply: Option<Message>) {
        match tag {
            Tag::NameOwner => {
                let owner = reply
                    .as_ref()
                    .filter(|r| !r.is_error())
                    .and_then(|r| r.args()?.str().map(str::to_string));
                match owner {
                    Some(o) if !o.is_empty() => self.bluez_up(&o),
                    _ => {
                        if self.bluez_owner.is_empty() {
                            log::warn!("bt: bluetoothd is not running; waiting for it");
                        }
                    }
                }
            }
            Tag::ManagedObjects(owner) => {
                if owner == self.bluez_owner {
                    self.on_managed_objects(reply);
                }
            }
            Tag::RegisterAgent => {
                if let Some(why) = failed(&reply)
                    && error_name(&reply) != "org.bluez.Error.AlreadyExists"
                {
                    log::error!("bt: RegisterAgent failed > {why}; no pairing possible");
                    return;
                }
                if let Some(mut m) =
                    self.bluez("/org/bluez", IFACE_AGENT_MANAGER, "RequestDefaultAgent")
                {
                    m.append().path(AGENT_PATH);
                    self.call(m, CALL_MS, Tag::DefaultAgent);
                }
            }
            Tag::DefaultAgent => {
                if let Some(why) = failed(&reply) {
                    log::error!("bt: RequestDefaultAgent failed > {why}");
                    return;
                }
                self.agent_registered = true;
                log::info!("bt: pairing agent registered ({AGENT_CAPABILITY}, default)");
            }
            Tag::StartDiscovery(owner) => {
                if owner != self.bluez_owner {
                    return;
                }
                // InProgress: this connection's discovery is already running.
                match failed(&reply) {
                    None => self.scan_state = ScanState::On,
                    Some(_) if error_name(&reply) == "org.bluez.Error.InProgress" => {
                        self.scan_state = ScanState::On;
                    }
                    Some(why) => {
                        log::warn!("bt: StartDiscovery failed > {why}");
                        self.scan_state = ScanState::Off;
                        if self.scan_wanted {
                            self.scan_wanted = false;
                            self.set_status("Could not search", STATUS_SHORT_MS);
                        }
                    }
                }
                self.sync_scan();
            }
            Tag::StopDiscovery(owner) => {
                if owner != self.bluez_owner {
                    return;
                }
                // Failed ("No discovery started") and NotReady: not running
                // either.
                if let Some(why) = failed(&reply) {
                    let name = error_name(&reply);
                    if name != "org.bluez.Error.Failed" && name != "org.bluez.Error.NotReady" {
                        log::warn!("bt: StopDiscovery failed > {why}");
                    }
                }
                self.scan_state = ScanState::Off;
                self.sync_scan();
            }
            Tag::Pair { seq, path, name } => {
                if seq == self.pair_seq && self.window.pair_pending() {
                    self.on_pair_reply(&path, &name, reply);
                }
            }
            Tag::Connect(path) => {
                if self.connecting.as_deref() == Some(path.as_str()) {
                    self.connecting = None;
                }
                if let Some(why) = failed(&reply) {
                    log::warn!("bt: connect {path} failed > {why}");
                    // Most keyboards only connect themselves, from a key press.
                    self.set_status(
                        "Could not connect - press a key on the device",
                        STATUS_LONG_MS,
                    );
                }
            }
            Tag::ConnectPhone(path) => {
                if self.connecting.as_deref() == Some(path.as_str()) {
                    self.connecting = None;
                }
                if let Some(why) = failed(&reply) {
                    log::warn!("bt: connect {path} failed > {why}");
                    // The controller can start a connection and never report
                    // its end: the page stays pending, every later attempt is
                    // "busy" and the phone never comes. A Disconnect cancels
                    // it, so the session's next try starts clean.
                    let connected = self.devices.get(&path).is_some_and(|d| d.connected);
                    if !connected {
                        self.device_call(
                            &path,
                            "Disconnect",
                            CALL_MS,
                            Tag::LogFailure("Disconnect"),
                        );
                    }
                }
            }
            Tag::ConnectAfterPair(path) => {
                if let Some(why) = failed(&reply) {
                    log::info!("bt: connect after pairing {path} > {why}");
                }
            }
            Tag::RemoveDevice(name) => {
                let Some(name) = name else {
                    if let Some(why) = failed(&reply) {
                        log::warn!("bt: RemoveDevice failed > {why}");
                    }
                    return;
                };
                if let Some(why) = failed(&reply) {
                    log::warn!("bt: RemoveDevice failed > {why}");
                    self.set_status(&format!("Could not forget {name}"), STATUS_LONG_MS);
                } else {
                    self.set_status(&format!("Forgot {name}"), STATUS_SHORT_MS);
                }
            }
            Tag::RegisterProfile(id) => {
                let Some(p) = self.profiles.iter_mut().find(|p| p.id == id) else {
                    // Unregistered before BlueZ answered: a registration
                    // that went through would hold the UUID for good.
                    if failed(&reply).is_none() {
                        self.unregister_path(&format!("/lehu/rfcomm/{id}"));
                    }
                    return;
                };
                p.pending = false;
                match failed(&reply) {
                    Some(why) if error_name(&reply) != "org.bluez.Error.AlreadyExists" => {
                        log::warn!("bt: RegisterProfile {} failed > {why}", p.profile.uuid);
                    }
                    _ => {
                        p.registered = true;
                        log::info!(
                            "bt: profile {} ({}) registered",
                            p.profile.name,
                            if p.profile.role == Role::Server {
                                "server"
                            } else {
                                "client"
                            }
                        );
                    }
                }
            }
            Tag::LogFailure(what) => {
                if let Some(why) = failed(&reply) {
                    log::warn!("bt: {what} failed > {why}");
                }
            }
            Tag::Discoverable => {
                self.discoverable_sent = false;
                if let Some(why) = failed(&reply) {
                    if why.contains("Busy") {
                        log::debug!("bt: Discoverable is being changed already");
                    } else {
                        log::warn!("bt: Discoverable failed > {why}");
                    }
                }
            }
        }
    }

    // ── commands ──────────────────────────────────────────────────────────

    fn start_scan(&mut self) {
        if self.scan_blocked() {
            self.set_status("Not during a session", STATUS_SHORT_MS);
            return;
        }
        if !self.ready() {
            self.set_status("Bluetooth is off", STATUS_SHORT_MS);
            return;
        }
        self.scan_until = clock::now_ms() + SCAN_MS;
        if self.scan_wanted {
            return;
        }
        self.scan_wanted = true;
        self.scan_number += 1;
        // Devices already reported by a discovery still running elsewhere.
        let n = self.scan_number;
        for d in self.devices.values_mut() {
            if d.rssi.is_some() {
                d.seen_scan = n;
            }
        }
        log::info!("bt: searching for input devices");
        self.sync_scan();
    }

    fn stop_scan(&mut self) {
        if !self.scan_wanted {
            return;
        }
        self.scan_wanted = false;
        self.sync_scan();
    }

    /// Move BlueZ towards `scan_wanted`; every reply calls this again, so a
    /// change of mind while a call is out is applied right after it.
    fn sync_scan(&mut self) {
        if self.adapter_path.is_empty() {
            return;
        }
        let owner = self.bluez_owner.clone();
        if self.scan_wanted && self.scan_state == ScanState::Off {
            self.scan_state = ScanState::Starting;
            // No discovery filter: classic keyboards rarely list the HID UUID
            // in their inquiry data, and any filter also reports
            // non-discoverable LE advertisers.
            self.adapter_call("StartDiscovery", None, Tag::StartDiscovery(owner));
        } else if !self.scan_wanted && self.scan_state == ScanState::On {
            self.scan_state = ScanState::Stopping;
            self.adapter_call("StopDiscovery", None, Tag::StopDiscovery(owner));
        }
    }

    fn do_pair(&mut self, path: &str) {
        if !self.devices.contains_key(path) || !self.ready() {
            return;
        }
        if self.window.pair_pending() {
            self.set_status("Busy - wait for the current pairing", STATUS_SHORT_MS);
            return;
        }
        // Quiet radio; the kernel would cancel the inquiry for the page anyway.
        self.stop_scan();
        self.window.begin(path);
        self.pair_seq += 1;
        let name = self.name_of(path);
        log::info!("bt: pairing {path} ({name})");
        self.device_call(
            path,
            "Pair",
            PAIR_CALL_MS,
            Tag::Pair {
                seq: self.pair_seq,
                path: path.to_string(),
                name,
            },
        );
    }

    fn on_pair_reply(&mut self, path: &str, name: &str, reply: Option<Message>) {
        self.window.pair_answered();
        let err = failed(&reply);
        let err_name = error_name(&reply).to_string();
        // A stored key is a finished pairing, even when the service search
        // behind Pair() never answered.
        let stored = self.devices.get(path).is_some_and(|d| d.bonded || d.paired);
        let ok = err.is_none() || err_name == "org.bluez.Error.AlreadyExists" || stored;
        let dialog_open = self.prompt_path == path && self.prompt_for != PromptFor::None;
        if self.window.user_cancelled() {
            self.window.abandon();
            if dialog_open {
                self.close_prompt(true);
            }
            if ok {
                // Cancel lost the race against the key exchange (and its
                // CancelPairing may already have dropped the key in the
                // controller): remove the half-made bond.
                log::info!("bt: pairing {name} cancelled after the key exchange; removing it");
                self.adapter_call("RemoveDevice", Some(path), Tag::RemoveDevice(None));
            }
            self.set_status("Pairing cancelled", STATUS_LONG_MS);
            return;
        }
        if ok {
            log::info!("bt: paired {name}");
            self.window.paired(clock::now_ms());
            // Trusted: classic reconnects skip authorization, and LE keeps
            // auto-connect across a Disconnect().
            self.set_bool(path, IFACE_DEVICE, "Trusted", true, "Trusted=true");
            if dialog_open {
                self.close_prompt(true);
            }
            self.connect_after_pair =
                Some((path.to_string(), clock::now_ms() + CONNECT_AFTER_PAIR_MS));
            self.set_status(&format!("Paired {name}"), STATUS_SHORT_MS);
            return;
        }
        log::warn!("bt: pairing {name} failed > {}", err.unwrap_or_default());
        self.window.abandon();
        if dialog_open {
            self.close_prompt(true);
        }
        let why = match err_name.as_str() {
            "org.bluez.Error.AuthenticationFailed" => " - wrong code",
            "org.bluez.Error.AuthenticationRejected" => " - rejected",
            "org.bluez.Error.AuthenticationTimeout" | "org.freedesktop.DBus.Error.NoReply" => {
                " - timed out"
            }
            "org.bluez.Error.ConnectionAttemptFailed" => " - not reachable",
            "org.bluez.Error.InProgress" => " - busy",
            _ => "",
        };
        self.set_status(&format!("Pairing failed{why}"), STATUS_LONG_MS);
    }

    fn do_connect(&mut self, path: &str, on: bool) {
        if !self.devices.contains_key(path) || !self.ready() {
            return;
        }
        // A Disconnect would cancel the service search behind Pair(), which
        // then never answers.
        if self.window.pair_pending() && self.window.target() == Some(path) {
            self.set_status("Busy - wait for the current pairing", STATUS_SHORT_MS);
            return;
        }
        if !on {
            self.device_call(path, "Disconnect", CALL_MS, Tag::LogFailure("Disconnect"));
            return;
        }
        self.connecting = Some(path.to_string());
        self.device_call(
            path,
            "Connect",
            CONNECT_CALL_MS,
            Tag::Connect(path.to_string()),
        );
    }

    /// Connect a phone. A plain Connect lets BlueZ pick the bearer, and
    /// phones advertise over Low Energy with the same address (the iPhone,
    /// the Samsung): BlueZ then tries LE, the attempt never completes on
    /// this controller, and while it is pending no classic connection gets
    /// through. A phone is connected through one of its classic services
    /// instead: the service of one of our client profiles (iAP2, which
    /// starts wireless CarPlay), or else its hands-free gateway, which the
    /// image's bluez-alsa answers as the car kit and which a phone takes as
    /// the car coming, wireless Android Auto included.
    fn connect_phone_at(&mut self, path: &str) {
        let Some(d) = self.devices.get(path) else {
            return;
        };
        let ours = self
            .profiles
            .iter()
            .filter(|p| p.profile.role == Role::Client)
            .map(|p| p.profile.uuid.to_ascii_lowercase());
        let theirs = [UUID_HFP_AG, UUID_HSP_AG].into_iter().map(str::to_string);
        let uuid = ours.chain(theirs).find(|u| d.uuids.contains(u));
        let Some(uuid) = uuid else {
            self.do_connect(path, true);
            return;
        };
        if !self.ready() {
            return;
        }
        log::info!("bt: connecting {path} through {uuid}");
        if let Some(mut m) = self.bluez(path, IFACE_DEVICE, "ConnectProfile") {
            m.append().str(&uuid);
            self.connecting = Some(path.to_string());
            self.call(
                m,
                PHONE_CONNECT_CALL_MS,
                Tag::ConnectPhone(path.to_string()),
            );
        }
    }

    /// BlueZ's object path of a device on our adapter.
    fn path_of(&self, address: Address) -> String {
        let a = address.0;
        format!(
            "{}/dev_{:02X}_{:02X}_{:02X}_{:02X}_{:02X}_{:02X}",
            self.adapter_path, a[0], a[1], a[2], a[3], a[4], a[5]
        )
    }

    fn do_forget(&mut self, path: &str) {
        let Some(d) = self.devices.get(path) else {
            return;
        };
        let p = Prompt::forget(&d.display_name());
        self.open_prompt(PromptFor::Forget, path, p);
    }

    fn do_prompt_accept(&mut self) {
        let path = self.prompt_path.clone();
        match self.prompt_for {
            PromptFor::Confirm => {
                if let Some(d) = self.deferred.take() {
                    self.reply_empty(&d);
                }
                self.close_prompt(false);
            }
            PromptFor::Forget => {
                self.close_prompt(false);
                if !self.ready() {
                    return;
                }
                let name = self.name_of(&path);
                if self.window.target() == Some(path.as_str()) {
                    // RemoveDevice leaves a pending Pair() unanswered.
                    self.window.abandon();
                }
                if self
                    .connect_after_pair
                    .as_ref()
                    .is_some_and(|(p, _)| *p == path)
                {
                    self.connect_after_pair = None;
                }
                log::info!("bt: forgetting {path} ({name})");
                // Disconnects, unpairs, deletes the stored key;
                // InterfacesRemoved then drops it from the list.
                self.adapter_call("RemoveDevice", Some(&path), Tag::RemoveDevice(Some(name)));
            }
            PromptFor::Code | PromptFor::None => {}
        }
    }

    fn do_prompt_cancel(&mut self) {
        match self.prompt_for {
            PromptFor::Code => {
                if self.window.pair_pending()
                    && self.window.target() == Some(self.prompt_path.as_str())
                {
                    // The Pair() reply settles it: an error, or, when the key
                    // got stored first, a bond that it removes again.
                    // CancelPairing only before that: once the key is stored
                    // it no longer cancels but silently unpairs in the
                    // controller.
                    self.window.cancel();
                    let path = self.prompt_path.clone();
                    let stored = self
                        .devices
                        .get(&path)
                        .is_some_and(|d| d.bonded || d.paired);
                    if !stored {
                        self.device_call(
                            &path,
                            "CancelPairing",
                            CALL_MS,
                            Tag::LogFailure("CancelPairing"),
                        );
                    }
                }
                self.close_prompt(false);
            }
            PromptFor::Confirm => {
                if self.deferred.is_some() {
                    self.window.cancel();
                }
                // "no" to the pending confirmation.
                self.close_prompt(true);
            }
            PromptFor::Forget => self.close_prompt(false),
            PromptFor::None => {}
        }
    }

    fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::Scan(true) => self.start_scan(),
            Command::Scan(false) => self.stop_scan(),
            Command::Pair(id) => self.do_pair(&id),
            Command::Connect(id, on) => self.do_connect(&id, on),
            Command::Forget(id) => self.do_forget(&id),
            Command::RemovePhone(address) => {
                let path = self.path_of(address);
                if self.ready() && self.devices.contains_key(&path) {
                    self.adapter_call("RemoveDevice", Some(&path), Tag::RemoveDevice(None));
                }
            }
            Command::ConnectPhone(address) => {
                let path = self.path_of(address);
                self.connect_phone_at(&path);
            }
            Command::BlockPhone(address, on) => {
                let path = self.path_of(address);
                if self.ready() && self.devices.contains_key(&path) {
                    let what = if on { "Blocked=true" } else { "Blocked=false" };
                    self.set_bool(&path, IFACE_DEVICE, "Blocked", on, what);
                }
            }
            Command::PromptAccept => self.do_prompt_accept(),
            Command::PromptCancel => self.do_prompt_cancel(),
            Command::ScreenVisible(false) => self.stop_scan(),
            Command::ScreenVisible(true) => {}
            Command::SessionActive(active) => {
                self.session_active = active;
                if active {
                    self.stop_scan();
                }
            }
            Command::SetName(name) => {
                if name != self.name {
                    self.name = name;
                    if !self.adapter_path.is_empty()
                        && !self.name.is_empty()
                        && self.alias != self.name
                    {
                        log::info!("bt: the adapter is called {} from now on", self.name);
                        let (path, name) = (self.adapter_path.clone(), self.name.clone());
                        self.set_str(&path, IFACE_ADAPTER, "Alias", &name, "Alias");
                    }
                }
            }
            Command::Register(id, profile, accepted) => self.do_register(id, profile, accepted),
            Command::Unregister(id) => self.do_unregister(id),
        }
    }

    // ── the loop ──────────────────────────────────────────────────────────

    fn run_deadlines(&mut self) {
        let now = clock::now_ms();
        if self.scan_wanted && now >= self.scan_until {
            log::info!("bt: search finished");
            self.stop_scan();
        }
        if let Some((path, at)) = &self.connect_after_pair
            && now >= *at
        {
            let path = path.clone();
            self.connect_after_pair = None;
            // Harmless when HID is already up (classic keyboards usually
            // connect themselves); LE devices just re-arm auto-connect.
            self.device_call(
                &path,
                "Connect",
                CONNECT_CALL_MS,
                Tag::ConnectAfterPair(path.clone()),
            );
        }
        if self
            .transient
            .as_ref()
            .is_some_and(|(_, until)| now >= *until)
        {
            self.transient = None;
        }
        self.window.expire(now);
    }

    /// The nearest moment something is due, in milliseconds from now.
    fn next_timeout_ms(&self) -> Option<u64> {
        let now = clock::now_ms();
        let mut next: Option<u64> = None;
        let mut consider = |deadline: u64| {
            let left = deadline.saturating_sub(now);
            next = Some(next.map_or(left, |n| n.min(left)));
        };
        if self.scan_wanted {
            consider(self.scan_until);
        }
        if let Some((_, at)) = &self.connect_after_pair {
            consider(*at);
        }
        if let Some((_, until)) = &self.transient {
            consider(*until);
        }
        if let Some(d) = self.window.deadline() {
            consider(d);
        }
        if self.bus.is_none()
            && let Some(at) = self.bus_retry_at
        {
            consider(at);
        }
        if let Some(bus) = &self.bus
            && let Some(ms) = bus.next_timeout_ms()
        {
            consider(now + ms);
        }
        next
    }

    fn snapshot(&self) -> Snapshot {
        let mut s = Snapshot::default();
        let prefix = format!("{}/", self.adapter_path);
        let mut order: Vec<(u32, &String, &Known)> = Vec::new();
        if self.ready() {
            for (path, d) in &self.devices {
                if path.starts_with(&prefix) && d.is_hid() {
                    order.push((d.order, path, d));
                }
            }
        }
        order.sort_by_key(|(o, _, _)| *o);
        let mut connected = 0;
        for (_, path, d) in order {
            let mut row = Device {
                id: path.clone(),
                name: d.display_name(),
                detail: String::new(),
                connected: false,
            };
            if d.paired || d.bonded {
                row.connected = d.connected;
                row.detail = if d.connected {
                    "Connected"
                } else {
                    "Not connected"
                }
                .to_string();
                if let Some(b) = d.battery {
                    row.detail.push_str(&format!("  {b}%"));
                }
                connected += u32::from(d.connected);
                s.paired.push(row);
            } else if self.scan_number != 0 && d.seen_scan == self.scan_number {
                row.detail = d.kind().text().to_string();
                if let Some(r) = d.rssi {
                    row.detail.push_str("  ");
                    row.detail.push_str(device::signal_text(r));
                }
                s.found.push(row);
            }
        }
        s.scanning = self.scan_wanted;
        s.scan_allowed = !self.scan_blocked();
        s.phone_connected = self.ready() && self.phone_connected();
        if let Some((text, _)) = &self.transient {
            s.status = text.clone();
        } else if self.window.pair_pending()
            && let Some(t) = self.window.target()
        {
            s.status = format!("Pairing {}...", self.name_of(t));
        } else if let Some(c) = &self.connecting {
            s.status = format!("Connecting {}...", self.name_of(c));
        }
        s.summary = if !self.ready() {
            "Off".to_string()
        } else if connected > 0 {
            format!("{connected} connected")
        } else if !s.paired.is_empty() {
            format!("{} paired", s.paired.len())
        } else {
            "None".to_string()
        };
        s.prompt = self.prompt.clone();
        s
    }

    fn publish(&mut self) {
        let s = self.snapshot();
        if s == self.last {
            return;
        }
        self.last = s.clone();
        *self
            .published
            .snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = s;
        self.published.generation.fetch_add(1, Ordering::AcqRel);
        (self.sink)(BtEvent::Changed);
    }

    fn run(mut self) {
        self.watch_bus_socket();
        let mut fds: Vec<libc::pollfd> = Vec::with_capacity(8);
        while self.running() {
            let retry_due = self.bus_retry_at.is_some_and(|at| clock::now_ms() >= at);
            if self.bus.is_none() && (self.bus_try || retry_due) {
                self.connect_bus();
            }
            // Signals first: a command must see the state BlueZ already
            // reported (e.g. a Cancel queued behind the key being stored).
            if let Some(bus) = &self.bus {
                bus.dispatch();
                while let Some(item) = self.bus.as_ref().and_then(Bus::take) {
                    match item {
                        Incoming::Reply { tag, reply } => self.on_reply(tag, reply),
                        Incoming::Call(m) if m.path() == AGENT_PATH => self.on_agent(m),
                        Incoming::Call(m) => self.on_profile(m),
                        Incoming::Signal(m) => self.on_signal(m),
                    }
                }
            }
            if self
                .bus
                .as_ref()
                .is_some_and(|b| self.bus_lost || !b.connected())
            {
                self.bus_lost = true;
                self.disconnect_bus();
                // The daemon may still be there (it dropped only us): try
                // again shortly; otherwise its socket coming back wakes us.
                self.bus_retry_at = Some(clock::now_ms() + BUS_RETRY_MS);
                // The loss is logged already.
                self.bus_wait_logged = true;
            }
            // Without a bus, commands still run and find Bluetooth off.
            while let Some(cmd) = self.commands.queue.pop() {
                self.on_command(cmd);
            }
            self.run_deadlines();
            self.publish();
            if !self.running() {
                break;
            }

            fds.clear();
            fds.push(libc::pollfd {
                fd: self.commands.wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            let waiting_for_bus = self.bus.is_none() && self.inotify.is_some();
            if waiting_for_bus && let Some(i) = &self.inotify {
                fds.push(libc::pollfd {
                    fd: i.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            let first = fds.len();
            if let Some(bus) = &mut self.bus {
                bus.poll_fds(&mut fds);
            }
            let timeout = self
                .next_timeout_ms()
                .map_or(-1, |ms| ms.min(i32::MAX as u64) as libc::c_int);
            // SAFETY: the pointer and length describe the Vec's live storage.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                log::error!("bt: poll failed ({e})");
                break;
            }
            if fds[0].revents & libc::POLLIN != 0 {
                self.commands.wake.drain();
            }
            if waiting_for_bus && fds.get(1).is_some_and(|p| p.revents & libc::POLLIN != 0) {
                self.on_inotify();
            }
            if let Some(bus) = &mut self.bus {
                bus.handle(&fds[first..]);
            }
        }
        self.disconnect_bus();
        for p in &self.profiles {
            p.accepted.close();
        }
        log::info!("bt: stopped");
    }
}

fn add_watch(fd: &OwnedFd, dir: &str) -> i32 {
    let Ok(c) = std::ffi::CString::new(dir) else {
        return -1;
    };
    // SAFETY: a live inotify descriptor and a NUL-terminated path.
    unsafe {
        libc::inotify_add_watch(
            fd.as_raw_fd(),
            c.as_ptr(),
            libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_ONLYDIR,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// The manager runs without a bus (the host may have none, or one
    /// without bluetoothd) and shows Bluetooth as off; commands do not
    /// block; a profile registers and unregisters cleanly.
    #[test]
    fn starts_without_a_bus_and_answers_commands() {
        let (tx, rx) = mpsc::channel();
        let m = Manager::start(&Bluetooth::default(), move |e| {
            let _ = tx.send(e);
        })
        .expect("a thread");
        let snap = m.snapshot();
        assert_eq!(snap.summary, "Off");
        assert!(snap.scan_allowed);
        m.scan(true);
        m.pair("/org/bluez/hci0/dev_00_11_22_33_44_55");
        m.prompt_cancel();
        m.screen_visible(false);
        let handle = m
            .register_rfcomm(RfcommProfile::server(
                "00001101-0000-1000-8000-00805f9b34fb",
                "Test",
                29,
            ))
            .expect("running");
        assert!(handle.try_accept().is_none());
        assert_eq!(handle.accept(Duration::from_millis(10)).map(|_| ()), None);
        // The scan either found no adapter ("Bluetooth is off") or was
        // started on this host's bluetoothd; both show on the status line
        // or the scanning flag within a moment.
        let mut seen = false;
        for _ in 0..50 {
            let s = m.snapshot();
            if s.status == "Bluetooth is off" || s.scanning || s.status == "Not during a session" {
                seen = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(seen, "the scan command was handled: {:?}", m.snapshot());
        drop(handle);
        m.stop();
        // Changed events came as the snapshot moved.
        assert!(rx.try_iter().any(|e| e == BtEvent::Changed));
    }

    /// Against the host's own bluetoothd, when it has one: the device tree
    /// is read and the adapter known within a few seconds. Run by hand
    /// with `--ignored --nocapture`; a host without Bluetooth shows Off.
    #[test]
    #[ignore = "needs the host's bluetoothd"]
    fn reads_the_hosts_device_tree() {
        let m = Manager::start(&Bluetooth::default(), |_| {}).expect("a thread");
        let mut snap = m.snapshot();
        for _ in 0..100 {
            snap = m.snapshot();
            if snap.summary != "Off" {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        println!("adapter {:?}: {snap:#?}", m.adapter_address());
        assert_ne!(
            snap.summary, "Off",
            "no adapter on this host, or no policy for us"
        );
        m.stop();
    }
}
