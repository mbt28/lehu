//! The one phone session, as the loop drives it: which protocol runs, what
//! is in front (the phone's video or the screens), the decoder that lives
//! exactly as long as the session's video, and what the loop hands on from
//! the input thread. Nothing here blocks: the protocols run on their own
//! threads and reach the loop through the session ports.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use lehu_core::clock;
use lehu_core::devices::{self, Registry};
use lehu_core::poll::Wake;
use lehu_core::queue::Queue;
use lehu_core::settings::{Settings, Wifi};
use lehu_drm::Display;
use lehu_input::{Contact, MAX_SLOTS, PhoneButton, Value};
use lehu_session::{Cmd, Event, Fault, Guidance, Kind, Link, NowPlaying, Phase, Ports, Session};
use lehu_video::{Codec, Decoder, Options};

use crate::audio::Audio;

/// How many access units wait for the decoder: the protocol pushes with a
/// depth of 4 and waits up to 500 ms before the oldest goes, so the queue
/// itself never fills.
const VIDEO_DEPTH: usize = 8;
/// Units kept for reuse once the decoder is done with them: each holds the
/// pages of the biggest frame it carried, so a few, not a queue's worth.
const RECYCLE_DEPTH: usize = 4;

/// A session that ended with its phone still plugged in starts over after
/// this: the phone's USB settles, and a phone that is leaving has left.
const RESTART_AFTER_MS: u64 = 2000;
/// A phone's USB node may still be root's for a moment after it appears,
/// until udev gives it to our group; given up after this.
const USB_WAIT_MS: u64 = 5000;
/// The system clock is left alone within this of the phone's.
const CLOCK_SLACK_MS: i64 = 2000;
/// A decoder that gave up is started again at most this often.
const RECOVER_EVERY_MS: u64 = 5000;
/// A session's phone gone from the bus this long is a pulled cable, not
/// the configuration switch's re-enumeration (under a second).
const UNPLUG_GRACE_MS: u64 = 2500;
/// An accessory on the bus this soon after start was left by an earlier
/// run (the USB scan reports it in the first turn).
const LEFTOVER_WINDOW_MS: u64 = 3000;
/// The detach and attach within this of an Android Auto session's end are
/// the reset that ended it, not the user's.
const RESET_WINDOW_MS: u64 = 3000;
/// A phone picked with Connect has this long to come.
const SWITCH_WAIT_MS: u64 = 30_000;
/// The first connect waits this long after the phone was let back: BlueZ
/// answers "busy" to a connect in the same moment as the unblock.
const FIRST_TRY_MS: u64 = 1500;
/// A connect that went unanswered is tried again this often.
const RETRY_MS: u64 = 8000;
/// "Switch to <phone>?" stands this long unanswered, then goes.
const QUESTION_MS: u64 = 20_000;
/// A status note ("<phone> did not connect") shows this long.
const NOTE_MS: u64 = 8000;
/// A hold protects the session that replaced the held phone, and the
/// moment after a Stop: with no phone in session this long, every held
/// phone is let back and may connect by itself again.
const HOLD_IDLE_MS: u64 = 30_000;

/// A phone the driver waits on or asks about: its registry id and name.
#[derive(Clone, Debug)]
struct Pending {
    id: String,
    name: String,
    until: u64,
    /// When to ask Bluetooth to connect the phone (again); 0 never.
    next_try: u64,
}

/// What is on the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Front {
    Screens,
    Phone,
}

/// Whether `device`'s usbfs node opens read-write for this process.
fn usb_writable(device: &lehu_usb::Device) -> bool {
    let Ok(path) = std::ffi::CString::new(device.node()) else {
        return false;
    };
    // SAFETY: a valid C string, read only by the call.
    unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
}

/// The phone on the bus, before and while a protocol runs on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plugged {
    pub device: lehu_usb::Device,
}

/// A session waiting for a phone that has not come yet (the wireless
/// listeners): its own pipes, read until it leaves Spawned, when it
/// becomes the session and the other waiters go.
struct Waiter {
    session: Box<dyn Session>,
    ports: Ports,
}

pub struct Driver {
    ports: Ports,
    /// The wireless sessions waiting for phones, CarPlay's and Android
    /// Auto's: both listen, neither has a phone; a cable ends them.
    waiters: Vec<Waiter>,
    display: Option<Arc<Display>>,
    low_delay: bool,
    fast: bool,
    active: Option<Box<dyn Session>>,
    phase: Phase,
    /// How far the active session got: a CarPlay Ultra session that met
    /// its phone and never went live falls back to classic.
    reached: Phase,
    /// The header names CarPlay Ultra: redrawn when the session becomes it.
    shown_ultra: bool,
    codec: Option<Codec>,
    decoder: Option<Decoder>,
    /// Set by the decoder thread when it needs a keyframe; the loop sends
    /// the command, since the session is the loop's.
    wants_keyframe: Arc<AtomicBool>,
    wake: Arc<Wake>,
    front: Front,
    /// Where to go back to after a call or the assistant brought the phone
    /// forward over the screens.
    restore: Option<Front>,
    plugged: Option<Plugged>,
    status: String,
    changed: bool,
    audio: Arc<Audio>,
    /// CarPlay's configuration, when the source allows it.
    carplay: Option<lehu_cp::Config>,
    /// Android Auto's configuration.
    android: lehu_aa::Config,
    /// When the driver started: an accessory seen in the first moments
    /// was left behind by an earlier run and is reset, not started on.
    started_at: u64,
    /// The loop ended the Android Auto session itself: the phone is not
    /// switched again until it is unplugged and plugged back. The reset
    /// that ends every Android Auto session re-enumerates the phone within
    /// moments; a detach later than that is the hand on the cable.
    aa_hold: bool,
    aa_closed_at: u64,
    /// We switched a phone to accessory mode: the accessory that follows
    /// is ours to start, not a left-over of an earlier run.
    switched: bool,
    /// The registry id of the wired Android Auto phone `aa_hold` holds.
    held_aa_wired: Option<String>,
    /// When the last session ended: a detach within moments of it is the
    /// phone's own re-enumeration, not a hand on the cable.
    closed_at: u64,
    /// Connect was picked on a phone: the one being waited for.
    switching: Option<Pending>,
    /// "Switch to <phone>?": a known phone came while another is in session.
    question: Option<Pending>,
    /// The question brought the screens over the phone's video.
    question_backgrounded: bool,
    /// Bumped when the question appears or goes, for the screens.
    question_gen: u32,
    /// A line for the status for a while ("<phone> did not connect").
    note: Option<(String, u64)>,
    /// Since when no phone has been in session: held phones are let back
    /// once that lasted `HOLD_IDLE_MS`.
    idle_since: Option<u64>,
    /// Wireless CarPlay: the Bluetooth manager and the access point's
    /// settings, when the source is carplay-wireless.
    bt: Option<Arc<lehu_bt::Manager>>,
    wifi: Option<Wifi>,
    /// The serial of the iPhone the CarPlay session runs on: the
    /// configuration switch re-enumerates it, so it comes back at another
    /// address and must not get a second session.
    cp_serial: Option<String>,
    /// When to start over on a plugged phone whose session ended.
    restart_at: Option<u64>,
    /// A phone on the cable whose node we may not open yet: the session
    /// that runs is left alone until it is ours, or given up at `.1`.
    usb_wait: Option<(lehu_usb::Device, u64)>,
    /// Whether a phone's node opens read-write for us (replaced in tests).
    node_writable: fn(&lehu_usb::Device) -> bool,
    /// `system.on-connect-script` and `on-disconnect-script`: run when a
    /// phone's session is identified, and when it ends; empty means none.
    on_connect: String,
    on_disconnect: String,
    /// `system.set-clock-command`, with `{unix}` for the phone's time.
    set_clock: String,
    /// The phones the head unit knows, and what the links show of them.
    registry: Registry,
    /// The phones connected over Bluetooth right now.
    bt_phones: Vec<lehu_bt::Address>,
    /// The USB serial of the plugged phone, for the registry.
    plugged_serial: Option<String>,
    /// The session's phone left the bus: unless it is back by then (the
    /// configuration switch re-enumerates it), the session ends.
    unplugged_at: Option<u64>,
    /// The connect script ran for this session; the disconnect one follows.
    announced: bool,
    /// The camera has the video memory: the decoder is built when it goes.
    parked: bool,
    /// When a failed decoder was last started again.
    recovered_at: u64,
    /// The phone had the panel when the camera took it, and was told so;
    /// it gets it back after.
    phone_had_screen: bool,
    /// What the session's phone plays and its route guidance, copied from
    /// the ports' side channel on the events that name them: the
    /// generation seen there, and our own, which also moves when a
    /// session's end clears them.
    now_playing: NowPlaying,
    np_seen: u32,
    np_gen: u32,
    guidance: Guidance,
    guide_seen: u32,
    guide_gen: u32,
    /// The album art's generation seen in the side channel, and the
    /// thread that decodes it for the Media page.
    art_seen: u32,
    art: crate::art::ArtDecoder,
    /// The phone's time zone and daylight saving, minutes east of UTC,
    /// once it told them (CarPlay's clock): the arrival as a time of day.
    zone_min: Option<i32>,
}

// `ports`, `adopt`, `kind`, `phase` and `front` are the protocol starts'
// side, which phases 4 and 5 bring; until then only the tests call them.
#[allow(dead_code)]
impl Driver {
    pub fn new(
        wake: Arc<Wake>,
        display: Option<Arc<Display>>,
        settings: &Settings,
        audio: Arc<Audio>,
        bt: Option<Arc<lehu_bt::Manager>>,
        state_dir: PathBuf,
    ) -> Self {
        let video = Arc::new(Queue::new(VIDEO_DEPTH));
        let recycle = Arc::new(Queue::new(RECYCLE_DEPTH));
        // What the system decodes decides what each phone is asked for.
        let decoders = lehu_video::caps();
        // Every protocol listens at once: a cable has the first claim, a
        // wireless session waits for phones meanwhile.
        let carplay = {
            let (w, h) = display
                .as_ref()
                .map_or((800, 480), |d| (d.width(), d.height()));
            let (w_mm, h_mm) = display.as_ref().map_or((0, 0), |d| d.size_mm());
            let mut config = lehu_cp::Config::from_settings(
                settings,
                lehu_video::carplay_codec(decoders, settings.display.codec),
                w,
                h,
                w_mm,
                h_mm,
            );
            config.fit_decoder(decoders);
            config.state_dir = state_dir.clone();
            Some(config)
        };
        let wifi = Some(settings.wifi.clone());
        let mut android = lehu_aa::Config::from_settings(settings);
        android.offer.fit(decoders, settings.display.codec);
        if let Some(cp) = &carplay {
            let (w, h) = android.offer.size();
            let offered: Vec<&str> = android.offer.codecs().map(Codec::name).collect();
            log::info!(
                "video: carplay is asked for {} at {}x{}, android auto offered {} at {w}x{h}",
                cp.codec,
                cp.width,
                cp.height,
                offered.join(" then ")
            );
        }
        // A keyframe request restarts Android Auto's stream (its focus taken
        // and given back): the driver asks only when a decoder gave up.
        android.keyframe_by_focus = true;
        let art = crate::art::ArtDecoder::new(Arc::clone(&wake));
        Self {
            ports: Ports::new(Arc::clone(&wake), video, recycle),
            waiters: Vec::new(),
            display,
            low_delay: settings.decode.low_delay,
            fast: settings.decode.fast,
            active: None,
            phase: Phase::Closed,
            reached: Phase::Closed,
            shown_ultra: false,
            codec: None,
            decoder: None,
            wants_keyframe: Arc::new(AtomicBool::new(false)),
            wake,
            front: Front::Screens,
            restore: None,
            plugged: None,
            status: String::from("No phone"),
            changed: true,
            audio,
            carplay,
            android,
            started_at: clock::now_ms(),
            aa_hold: false,
            aa_closed_at: 0,
            switched: false,
            held_aa_wired: None,
            closed_at: 0,
            switching: None,
            question: None,
            question_backgrounded: false,
            question_gen: 0,
            note: None,
            idle_since: None,
            bt,
            // The wireless session waits for phones from the start; the
            // first turn starts it (after the USB scan, so a cable wins).
            restart_at: wifi.as_ref().map(|_| clock::now_ms()),
            usb_wait: None,
            node_writable: usb_writable,
            wifi,
            cp_serial: None,
            on_connect: settings.system.on_connect_script.clone(),
            on_disconnect: settings.system.on_disconnect_script.clone(),
            set_clock: settings.system.set_clock_command.clone(),
            registry: Registry::open(&state_dir),
            bt_phones: Vec::new(),
            plugged_serial: None,
            unplugged_at: None,
            announced: false,
            parked: false,
            recovered_at: 0,
            phone_had_screen: false,
            now_playing: NowPlaying::default(),
            np_seen: 0,
            np_gen: 0,
            guidance: Guidance::default(),
            guide_seen: 0,
            guide_gen: 0,
            art_seen: 0,
            art,
            zone_min: None,
        }
    }

    /// What the session's phone plays, with a generation that moves with
    /// every change.
    pub fn now_playing(&self) -> (u32, &NowPlaying) {
        (self.np_gen, &self.now_playing)
    }

    /// The session's route guidance, likewise.
    pub fn guidance(&self) -> (u32, &Guidance) {
        (self.guide_gen, &self.guidance)
    }

    /// The album art for the Media page, decoded, once each time it is
    /// ready: a picture, or none.
    pub fn take_art(&self) -> Option<lehu_ui::Art> {
        self.art.take()
    }

    /// The phone's offset from UTC in minutes, once it said.
    pub fn zone_min(&self) -> Option<i32> {
        self.zone_min
    }

    /// A transport key from the Media page: a whole press, as a key on a
    /// remote sends it.
    pub fn media_key(&self, button: PhoneButton) {
        self.command(Cmd::Key { button, down: true });
        self.command(Cmd::Key {
            button,
            down: false,
        });
    }

    /// Copy what the side channel holds of the session in front, when it
    /// moved on.
    fn read_side(&mut self) {
        if let Some(seen) = self
            .ports
            .now_playing
            .read_into(self.np_seen, &mut self.now_playing)
        {
            self.np_seen = seen;
            self.np_gen = self.np_gen.wrapping_add(1);
        }
        if let Some(seen) = self
            .ports
            .guidance
            .read_into(self.guide_seen, &mut self.guidance)
        {
            self.guide_seen = seen;
            self.guide_gen = self.guide_gen.wrapping_add(1);
        }
        // The art only when it moved: it is tens of kilobytes. The copy
        // goes to the decoder's thread and is dropped there.
        if self.ports.artwork.generation() != self.art_seen {
            let mut art = lehu_session::Artwork::default();
            if let Some(seen) = self.ports.artwork.read_into(self.art_seen, &mut art) {
                self.art_seen = seen;
                self.art.submit(art.data);
            }
        }
    }

    /// The session's ports changed (a new session, a waiter promoted): their
    /// side channel starts from its own generation 0.
    fn new_side(&mut self) {
        self.np_seen = 0;
        self.guide_seen = 0;
        // The new ports' art, if any, replaces the last session's.
        self.art_seen = 0;
        self.art.submit(Vec::new());
        self.read_side();
    }

    /// The session ended: what its phone played and the route it guided
    /// are gone with it.
    fn clear_side(&mut self) {
        if self.now_playing != NowPlaying::default() {
            self.now_playing.clear();
            self.np_gen = self.np_gen.wrapping_add(1);
        }
        if self.guidance != Guidance::default() {
            self.guidance.clear();
            self.guide_gen = self.guide_gen.wrapping_add(1);
        }
        self.art.submit(Vec::new());
    }

    /// The decoder is up (the phone's video can show).
    pub fn video_running(&self) -> bool {
        self.decoder.is_some()
    }

    pub fn phone_had_screen(&self) -> bool {
        self.phone_had_screen
    }

    /// The camera takes the panel: the phone, if it had it, is told (Android
    /// Auto stops its video); with `decoder_too` the decoder gives its CMA
    /// pool back as well. Fast switch keeps it when the memory holds both.
    pub fn park(&mut self, decoder_too: bool) {
        if !self.phone_had_screen && self.phone_front() {
            self.phone_had_screen = true;
            self.command(Cmd::VideoActive(false));
        }
        if decoder_too && self.decoder.is_some() {
            self.end_video();
        }
        if decoder_too {
            self.parked = true;
        }
    }

    /// The camera is gone: the decoder comes back for a live session, and
    /// the phone gets the panel back with a keyframe to fill it.
    pub fn unpark(&mut self) {
        if self.parked {
            self.parked = false;
            if let Some(codec) = self.codec
                && self.active()
            {
                self.start_video(codec);
            }
        }
        if self.phone_had_screen {
            self.phone_had_screen = false;
            if self.active() {
                self.command(Cmd::VideoActive(true));
                self.command(Cmd::Keyframe);
            }
        }
    }

    /// A fresh set of pipes for a session about to start: its own events
    /// and microphone queues, the shared video ones. A superseded session
    /// keeps reporting into its old queue while it winds down, which
    /// nobody reads, instead of into the new session's.
    fn fresh_ports(&mut self) -> Ports {
        self.ports = self.make_ports();
        self.new_side();
        self.ports.clone()
    }

    /// Pipes for a session of its own (a waiter), the video ones shared.
    fn make_ports(&self) -> Ports {
        Ports::new(
            Arc::clone(&self.wake),
            Arc::clone(&self.ports.video),
            Arc::clone(&self.ports.recycle),
        )
    }

    /// The waiting sessions end: a cable arrived, or the access point
    /// changed under them.
    fn end_waiters(&mut self, why: &str) {
        for w in self.waiters.drain(..) {
            log::info!("session: {:?} waiter {why}", w.session.kind());
            w.session.stop();
        }
    }

    /// A waiting session's phone arrived: it becomes the session, the
    /// other waiters go (one screen), and its first real event is handled.
    fn promote(&mut self, index: usize, event: Event) {
        let w = self.waiters.remove(index);
        self.end_waiters("makes room for the phone that came");
        self.end_session("superseded");
        self.restart_at = None;
        self.ports = w.ports;
        self.new_side();
        self.phase = Phase::Spawned;
        self.reached = Phase::Spawned;
        self.codec = None;
        self.front = Front::Screens;
        self.restore = None;
        log::info!(
            "session: {:?} over {:?} has its phone",
            w.session.kind(),
            w.session.link()
        );
        self.active = Some(w.session);
        self.on_event(event);
    }

    /// What the waiting sessions reported: a phone arriving promotes its
    /// session; a waiter that closed is dropped and started again later.
    fn drain_waiters(&mut self) {
        let mut promote = None;
        let mut closed = Vec::new();
        'waiters: for (i, w) in self.waiters.iter().enumerate() {
            while let Some(event) = w.ports.events.pop() {
                match event {
                    Event::Phase(p) if p >= Phase::Identified && p < Phase::Closing => {
                        promote = Some((i, event));
                        break 'waiters;
                    }
                    Event::Phase(Phase::Closed) => closed.push(i),
                    Event::Phase(_) => {}
                    Event::Fault(f) => {
                        log::warn!("session: a waiting {:?} reports {f:?}", w.session.kind());
                    }
                    other => {
                        log::debug!(
                            "session: a waiting {:?} reports {other:?}",
                            w.session.kind()
                        );
                    }
                }
            }
        }
        if let Some((i, event)) = promote {
            self.promote(i, event);
            return;
        }
        for i in closed.into_iter().rev() {
            if i < self.waiters.len() {
                let w = self.waiters.remove(i);
                log::info!("session: the waiting {:?} closed", w.session.kind());
                w.session.stop();
            }
        }
        if !self.active()
            && self.waiters.len() < 2
            && self.restart_at.is_none()
            && self.wifi.is_some()
        {
            self.restart_at = Some(clock::now_ms() + RESTART_AFTER_MS);
        }
    }

    /// A new session takes over: a stale one for the same phone is ended
    /// at once, as LIVI does, rather than left to time out.
    pub fn adopt(&mut self, session: Box<dyn Session>) {
        self.end_session("superseded");
        self.restart_at = None;
        self.phase = session.phase();
        self.reached = self.phase;
        self.codec = None;
        self.front = Front::Screens;
        self.restore = None;
        log::info!(
            "session: {:?} over {:?}, {:?}",
            session.kind(),
            session.link(),
            self.phase
        );
        self.active = Some(session);
        self.set_status();
    }

    pub fn active(&self) -> bool {
        self.active.is_some() && self.phase.alive()
    }

    pub fn kind(&self) -> Option<Kind> {
        self.active.as_ref().map(|s| s.kind())
    }

    pub fn link(&self) -> Option<Link> {
        self.active.as_ref().map(|s| s.link())
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn front(&self) -> Front {
        self.front
    }

    /// The phone's video is what the panel shows.
    pub fn phone_front(&self) -> bool {
        self.front == Front::Phone && self.active()
    }

    /// Whether the phone takes the buttons: it has the screen, or is about
    /// to (a session past identification with the screens up still gets
    /// its Menu press to come forward).
    pub fn takes_buttons(&self) -> bool {
        self.phone_front()
    }

    /// The status line for the screens; `true` once after each change.
    pub fn take_status(&mut self) -> Option<&str> {
        if std::mem::take(&mut self.changed) {
            Some(&self.status)
        } else {
            None
        }
    }

    pub fn plugged(&self) -> Option<Plugged> {
        self.plugged
    }

    fn set_status(&mut self) {
        let text = match (&self.active, self.plugged) {
            (Some(s), _) => {
                let phone = s.phone().unwrap_or("phone");
                let what = match s.kind() {
                    Kind::CarPlay if s.ultra() => "CarPlay Ultra",
                    Kind::CarPlay => "CarPlay",
                    Kind::AndroidAuto => "Android Auto",
                };
                match self.phase {
                    Phase::Spawned => format!("{what}: connecting"),
                    Phase::Identified | Phase::Ready => format!("{what}: {phone} starting"),
                    Phase::Live | Phase::Backgrounded => format!("{what}: {phone}"),
                    Phase::Closing => format!("{what}: {phone} leaving"),
                    Phase::Closed => "No phone".to_string(),
                }
            }
            (None, _) if self.switching.is_some() => format!(
                "Switching to {}...",
                self.switching.as_ref().map_or("", |p| p.name.as_str())
            ),
            (None, _) if self.note.is_some() => {
                self.note.as_ref().map_or(String::new(), |n| n.0.clone())
            }
            (None, Some(p)) => match p.device.role() {
                lehu_usb::Role::Apple => "iPhone plugged in".to_string(),
                lehu_usb::Role::Accessory => "Android phone plugged in".to_string(),
                _ => format!("USB device {}", p.device.id),
            },
            (None, None) => "No phone".to_string(),
        };
        if text != self.status {
            self.status = text;
            self.changed = true;
        }
    }

    /// A device came or went on the USB bus: an iPhone starts wired
    /// CarPlay; a phone leaving is noticed by its session itself (the link
    /// drops under it), so a detach only updates what is shown.
    pub fn usb(&mut self, event: lehu_usb::Event) {
        match event {
            lehu_usb::Event::Attached(d) => {
                if d.role() == lehu_usb::Role::Hub {
                    return;
                }
                log::info!("usb: {d} attached ({:?})", d.role());
                self.plugged = Some(Plugged { device: d });
                match d.role() {
                    lehu_usb::Role::Apple => {
                        if let Some(serial) = lehu_cp::usbmux::serial_of(d.name()) {
                            self.registry.usb_present(&serial, true);
                            if self.cp_serial.as_deref() == Some(serial.as_str()) {
                                self.unplugged_at = None; // the re-enumeration
                            }
                            self.plugged_serial = Some(serial);
                        }
                        self.start_carplay(d);
                    }
                    lehu_usb::Role::Accessory => self.start_android(d),
                    lehu_usb::Role::Other => self.switch_android(d),
                    lehu_usb::Role::Hub => {}
                }
            }
            lehu_usb::Event::Detached(d) => {
                let by_hand = clock::now_ms().saturating_sub(self.closed_at) > RESET_WINDOW_MS;
                if d.role() != lehu_usb::Role::Apple
                    && clock::now_ms().saturating_sub(self.aa_closed_at) > RESET_WINDOW_MS
                {
                    // Unplugged by hand, not the reset that ended the
                    // session: a phone we closed on may be switched again.
                    self.aa_hold = false;
                    if let Some(id) = self.held_aa_wired.take() {
                        self.release(&id);
                    }
                }
                if by_hand
                    && d.role() == lehu_usb::Role::Apple
                    && let Some(serial) = &self.plugged_serial
                    && let Some(id) = self
                        .registry
                        .by_usb(serial)
                        .filter(|r| r.held)
                        .map(|r| r.id.clone())
                {
                    // A held iPhone unplugged: plugged again, it is taken.
                    self.release(&id);
                }
                if self.plugged.is_some_and(|p| p.device.same_slot(&d)) {
                    log::info!("usb: {d} detached");
                    self.plugged = None;
                    if let Some(serial) = self.plugged_serial.take() {
                        self.registry.usb_present(&serial, false);
                        if self.active()
                            && self.link() == Some(Link::Usb)
                            && self.cp_serial.as_deref() == Some(serial.as_str())
                        {
                            self.unplugged_at = Some(clock::now_ms() + UNPLUG_GRACE_MS);
                        }
                    }
                }
            }
        }
        self.set_status();
    }

    /// Ultra is new ground with every iPhone: a session that offered it,
    /// met its phone and ended before the picture came is taken as Ultra
    /// failing, and the sessions after it offer classic CarPlay until the
    /// program starts again, so a phone that refuses Ultra still connects.
    fn ultra_fallback(&mut self) {
        let carplay = self
            .active
            .as_ref()
            .is_some_and(|s| s.kind() == Kind::CarPlay);
        let met = self.reached >= Phase::Identified && self.reached < Phase::Live;
        if let Some(cfg) = self.carplay.as_mut().filter(|c| c.ultra && carplay && met) {
            log::warn!(
                "carplay: the Ultra session ended before its picture; classic CarPlay from now on"
            );
            cfg.ultra = false;
        }
    }

    /// End the active session, whatever its kind, telling the phone.
    fn end_session(&mut self, why: &str) {
        if let Some(old) = self.active.take() {
            log::info!("session: {:?} over {:?} {why}", old.kind(), old.link());
            if old.kind() == Kind::AndroidAuto && old.link() == Link::Usb {
                self.aa_hold = true;
                self.aa_closed_at = clock::now_ms();
            }
            self.end_video();
            old.stop();
            self.audio.clear();
            self.clear_side();
            self.registry.session_ended();
            if std::mem::take(&mut self.announced) {
                run_script(&self.on_disconnect, "disconnect");
            }
        }
        self.phase = Phase::Closed;
        self.front = Front::Screens;
        self.restore = None;
        self.closed_at = clock::now_ms();
        crate::mem::trim();
    }

    /// Wired CarPlay on the plugged iPhone, unless the session already
    /// runs on it: the configuration switch re-enumerates the phone, and
    /// this is how it comes back. A cable takes over from a wireless
    /// session (one screen, and :7000 is one listener), never the reverse.
    fn start_carplay(&mut self, device: lehu_usb::Device) {
        let Some(config) = self.carplay.clone() else {
            log::info!("usb: an iPhone, but the source is not CarPlay");
            return;
        };
        let serial = lehu_cp::usbmux::serial_of(device.name());
        if let Some(s) = &serial
            && self.registry.by_usb(s).is_some_and(|d| d.held)
        {
            log::info!(
                "usb: the iPhone on the cable was stopped; it waits for Connect or a replug"
            );
            return;
        }
        if self.active()
            && self.link() == Some(Link::Usb)
            && serial.is_some()
            && serial == self.cp_serial
        {
            log::debug!("usb: the session's phone came back at {}", device.node());
            return;
        }
        if !self.cable_ready(&device) {
            return;
        }
        self.end_waiters("makes room for the cable");
        if self.active() && self.link() == Some(Link::Wifi) {
            self.end_session("makes room for the cable");
        }
        let ports = self.fresh_ports();
        let sinks = self.audio.sink(Arc::clone(&ports.mic), true);
        match lehu_cp::Session::start(config, &device, sinks, ports) {
            Ok(session) => {
                self.cp_serial = Some(session.serial().to_string());
                self.adopt(Box::new(session));
            }
            Err(e) => {
                log::warn!("carplay: cannot start on {device}: {e}");
                self.cp_serial = None;
            }
        }
    }

    /// Whether the phone on the cable can be taken now. A node we may not
    /// open yet (udev has not given it to our group, or the image has no
    /// rule for it) must not end the session that runs: the cable waits,
    /// and `drain` takes it once the node is ours, or gives up.
    fn cable_ready(&mut self, device: &lehu_usb::Device) -> bool {
        if (self.node_writable)(device) {
            if self.usb_wait.is_some_and(|(d, _)| d.same_slot(device)) {
                self.usb_wait = None;
            }
            return true;
        }
        if !self.usb_wait.is_some_and(|(d, _)| d.same_slot(device)) {
            log::info!(
                "usb: {device}: {} is not ours yet; the cable waits, what runs goes on",
                device.node()
            );
            self.usb_wait = Some((*device, clock::now_ms() + USB_WAIT_MS));
        }
        false
    }

    /// The waiting cable, once its node is ours; or given up.
    fn retry_cable(&mut self, now: u64) {
        let Some((device, until)) = self.usb_wait else {
            return;
        };
        if !self.plugged.is_some_and(|p| p.device.same_slot(&device)) {
            self.usb_wait = None;
            return;
        }
        if (self.node_writable)(&device) {
            self.usb_wait = None;
            log::info!("usb: {device}: its node is ours now");
            match device.role() {
                lehu_usb::Role::Apple => self.start_carplay(device),
                lehu_usb::Role::Accessory => self.start_android(device),
                lehu_usb::Role::Other => self.switch_android(device),
                lehu_usb::Role::Hub => {}
            }
            self.set_status();
        } else if now >= until {
            self.usb_wait = None;
            log::warn!(
                "usb: {device}: cannot open {} read-write; the cable is left alone until it is plugged in again (the image's USB rule gives phones to the plugdev group)",
                device.node()
            );
        }
    }

    /// An Android phone before the accessory switch: asked whether it
    /// speaks the accessory protocol and, if so, switched; it then
    /// re-enumerates as an accessory and `start_android` takes it.
    fn switch_android(&mut self, device: lehu_usb::Device) {
        if !self.android.accepts(device.id) {
            log::info!("usb: {device} is not the pinned Android phone");
            return;
        }
        if self.aa_hold {
            log::info!("usb: {device} stays as it is; its session was ended from here");
            return;
        }
        if !self.cable_ready(&device) {
            return;
        }
        // Switching a phone under its running wireless session fights it:
        // the session ends first (the cable comes first anyway).
        if self.active()
            && self.kind() == Some(Kind::AndroidAuto)
            && self.link() == Some(Link::Wifi)
        {
            self.end_session("makes room for the cable");
        }
        match lehu_aa::switch_to_accessory(&device) {
            Ok(lehu_aa::Switch::Switched) => {
                log::info!("usb: {device} switched to accessory mode, re-enumerating");
                self.switched = true;
            }
            Ok(lehu_aa::Switch::Skipped(what)) => log::debug!("usb: {device} skipped ({what})"),
            Ok(lehu_aa::Switch::NotSupported) => {
                log::info!("usb: {device} does not speak the accessory protocol");
            }
            Err(e) => log::warn!("usb: accessory switch on {device}: {e}"),
        }
    }

    /// Android Auto on a phone in accessory mode. A cable takes over from a
    /// wireless session, as with CarPlay. An accessory found in the first
    /// moments after start was left behind by an earlier run and may hold
    /// a half-open pipe: it is reset once instead, and started on when it
    /// comes back.
    fn start_android(&mut self, device: lehu_usb::Device) {
        if !self.cable_ready(&device) {
            return;
        }
        let leftover =
            clock::now_ms().saturating_sub(self.started_at) < LEFTOVER_WINDOW_MS && !self.switched;
        self.switched = false;
        if leftover {
            // A reset that finds the same descriptors re-enumerates nothing
            // (a Samsung stays an accessory), so the phone is started on
            // in a moment if it is still there.
            log::info!("usb: {device} is a left-over accessory; resetting it");
            if let Err(e) = lehu_usb::reset(&device) {
                log::warn!("usb: reset of {device}: {e}");
            }
            self.restart_at = Some(clock::now_ms() + RESTART_AFTER_MS);
            return;
        }
        self.end_waiters("makes room for the cable");
        if self.active() && self.link() == Some(Link::Wifi) {
            self.end_session("makes room for the cable");
        }
        let ports = self.fresh_ports();
        let sinks = self.audio.sink(Arc::clone(&ports.mic), false);
        match lehu_aa::Session::start(self.android.clone(), &device, sinks, ports) {
            Ok(session) => {
                self.cp_serial = None;
                self.adopt(Box::new(session));
            }
            Err(e) => log::warn!("android auto: cannot start on {device}: {e}"),
        }
    }

    /// Wireless CarPlay: the session that waits for phones over Bluetooth
    /// and hands them the access point. Without an access point up there
    /// is nothing to hand them: tried again in a while.
    fn start_wireless(&mut self) {
        let (Some(bt), Some(wifi)) = (self.bt.clone(), self.wifi.clone()) else {
            return;
        };
        let has = |kind: Kind| self.waiters.iter().any(|w| w.session.kind() == kind);
        if self.active() || (has(Kind::CarPlay) && has(Kind::AndroidAuto)) {
            return;
        }
        let ap = lehu_wifi::AccessPoint::read(&wifi);
        if !ap.valid() {
            log::warn!("wireless: the access point is not up; trying again in 5 s");
            self.restart_at = Some(clock::now_ms() + 5000);
            return;
        }
        let (want_cp, want_aa) = (!has(Kind::CarPlay), !has(Kind::AndroidAuto));
        if let Some(config) = self.carplay.clone().filter(|_| want_cp) {
            let ports = self.make_ports();
            let sinks = self.audio.sink(Arc::clone(&ports.mic), true);
            match lehu_cp::Session::start_wireless(config, &bt, ap.clone(), sinks, ports.clone()) {
                Ok(session) => self.waiters.push(Waiter {
                    session: Box::new(session),
                    ports,
                }),
                Err(e) => log::warn!("carplay: cannot wait for wireless phones: {e}"),
            }
        }
        if want_aa {
            let ports = self.make_ports();
            let sinks = self.audio.sink(Arc::clone(&ports.mic), false);
            match lehu_aa::Session::start_wireless(
                self.android.clone(),
                &bt,
                ap,
                sinks,
                ports.clone(),
            ) {
                Ok(session) => self.waiters.push(Waiter {
                    session: Box::new(session),
                    ports,
                }),
                Err(e) => log::warn!("android auto: cannot wait for wireless phones: {e}"),
            }
        }
        if self.waiters.is_empty() {
            self.restart_at = Some(clock::now_ms() + 10_000);
        } else {
            self.cp_serial = None;
            self.set_status();
        }
    }

    /// A wireless session is on with a phone: Bluetooth discovery is
    /// refused meanwhile.
    pub fn wireless_active(&self) -> bool {
        self.link() == Some(Link::Wifi)
            && self.phase >= Phase::Identified
            && self.phase < Phase::Closing
    }

    /// The access point's credentials changed: a wireless session hands
    /// them to the phone at its start, so it starts over.
    pub fn access_point_changed(&mut self) {
        if self.wifi.is_none() {
            return;
        }
        self.end_waiters("restarts with the new access point");
        if self.link() == Some(Link::Wifi) {
            self.end_session("restarts with the new access point");
        }
        self.restart_at = Some(clock::now_ms());
    }

    /// A phone connected over Bluetooth, or left: the registry shows it
    /// Available, and a wireless session identifying meanwhile is tied to
    /// the one phone connected.
    pub fn bt_phone(&mut self, address: lehu_bt::Address, connected: bool) {
        log::info!(
            "bluetooth: phone {address} {}",
            if connected { "connected" } else { "left" }
        );
        self.bt_phones.retain(|a| *a != address);
        if connected {
            self.bt_phones.push(address);
        }
        self.registry.bt_present(&address.to_string(), connected);
        if connected && self.active() && self.phase >= Phase::Identified {
            self.ask_about(address);
        } else if !connected
            && let Some(q) = &self.question
            && self
                .registry
                .by_id(&q.id)
                .is_some_and(|d| d.bt_address.eq_ignore_ascii_case(&address.to_string()))
        {
            self.withdraw_question(true);
        }
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The registry's Forget: the record goes, and so does the pairing on
    /// every link that holds one (the CarPlay store, BlueZ). A session on
    /// that phone ends.
    pub fn forget_device(&mut self, id: &str) {
        let Some(gone) = self.registry.forget(id) else {
            return;
        };
        log::info!("devices: forgetting {} ({id})", gone.name);
        if self.active.as_ref().and_then(|s| s.identity()).as_deref() == Some(id) {
            self.end_session("forgotten");
        }
        if gone.kind == devices::Kind::CarPlay
            && let Some(cfg) = &self.carplay
        {
            match lehu_cp::pairing::Store::open(&cfg.state_dir.join("carplay")) {
                Ok(store) => {
                    if let Err(e) = store.remove_pairing(id) {
                        log::warn!("devices: the CarPlay pairing: {e}");
                    }
                }
                Err(e) => log::warn!("devices: the CarPlay store: {e}"),
            }
        }
        if let (Some(bt), Some(address)) = (&self.bt, lehu_bt::Address::parse(&gone.bt_address)) {
            bt.remove_phone(address);
        }
    }

    /// The session's phone, by its registry id, once it said who it is.
    fn active_identity(&self) -> Option<String> {
        self.active.as_ref().and_then(|s| s.identity())
    }

    /// Hold the session's phone off before its session ends: it is not
    /// taken again by itself (its Bluetooth blocked, its cable ignored)
    /// until it is picked with Connect or plugged again.
    fn hold_current(&mut self) {
        let Some(s) = &self.active else { return };
        let Some(id) = s.identity() else { return };
        let address = s
            .bt_address()
            .or_else(|| self.registry.by_id(&id).map(|d| d.bt_address.clone()))
            .and_then(|a| lehu_bt::Address::parse(&a));
        let wired_aa = s.kind() == Kind::AndroidAuto && s.link() == Link::Usb;
        log::info!("devices: holding {id}");
        self.registry.hold(&id, true);
        if let (Some(bt), Some(address)) = (&self.bt, address) {
            bt.block_phone(address, true);
        }
        if wired_aa {
            self.held_aa_wired = Some(id);
        }
    }

    /// Let a held phone back: its Bluetooth unblocked, its cable taken.
    fn release(&mut self, id: &str) {
        if !self.registry.hold(id, false) {
            return;
        }
        log::info!("devices: {id} is no longer held");
        if let (Some(bt), Some(address)) = (
            &self.bt,
            self.registry
                .by_id(id)
                .and_then(|d| lehu_bt::Address::parse(&d.bt_address)),
        ) {
            bt.block_phone(address, false);
        }
        if self.held_aa_wired.as_deref() == Some(id) {
            self.held_aa_wired = None;
            self.aa_hold = false;
        }
    }

    /// No phone in session for `HOLD_IDLE_MS`: the holds have nothing left
    /// to protect, and a held phone would only look dead to its owner.
    fn release_when_idle(&mut self, now: u64) {
        if self.active() || self.switching.is_some() {
            self.idle_since = None;
            return;
        }
        let since = *self.idle_since.get_or_insert(now);
        if now.saturating_sub(since) < HOLD_IDLE_MS {
            return;
        }
        let held: Vec<String> = self
            .registry
            .all()
            .iter()
            .filter(|d| d.held)
            .map(|d| d.id.clone())
            .collect();
        for id in held {
            log::info!("devices: no phone in session; letting {id} back");
            self.release(&id);
        }
        self.idle_since = Some(now);
    }

    /// Android Auto's frame rate, live. The phone takes it when a session
    /// starts, so a running Android Auto session starts again on it (over
    /// its cable, or woken over Bluetooth, as Connect does); the listeners
    /// waiting meanwhile restart with it.
    pub fn set_android_fps(&mut self, fps: u32) {
        if self.android.offer.fps == fps {
            return;
        }
        self.android.offer.fps = fps;
        log::info!("android auto: {fps} fps from now on");
        if self.active() && self.kind() == Some(Kind::AndroidAuto) {
            let phone = self.active_identity();
            self.end_session("the frame rate changed");
            match phone {
                Some(id) => self.connect_device(&id),
                None => self.restart_at = Some(clock::now_ms()),
            }
        } else if !self.active() {
            self.end_waiters("the frame rate changed");
            self.restart_at = Some(clock::now_ms());
        }
        self.set_status();
    }

    /// The Devices screen's Stop: the session on that phone ends and the
    /// phone is held; the listeners open again for the others.
    pub fn stop_device(&mut self, id: &str) {
        if self.active_identity().as_deref() != Some(id) {
            return;
        }
        self.hold_current();
        self.end_session("stopped from the screens");
        self.restart_at = Some(clock::now_ms() + RESTART_AFTER_MS);
        self.set_status();
    }

    /// The Devices screen's Connect, and a yes to "Switch to ...?": the
    /// session on another phone ends and that phone is held; the picked
    /// one is let back and woken, over its cable when it is the one
    /// plugged in, else over Bluetooth, which starts its wireless session.
    pub fn connect_device(&mut self, id: &str) {
        let Some(d) = self.registry.by_id(id).cloned() else {
            return;
        };
        self.withdraw_question(false);
        self.release(id);
        if self.active_identity().as_deref() == Some(id) {
            return;
        }
        let name = if d.name.is_empty() {
            "the phone".to_string()
        } else {
            d.name.clone()
        };
        if self.active() {
            self.hold_current();
            self.end_session(&format!("makes room for {name}"));
        }
        self.end_waiters("restart for the picked phone");
        if let Some(p) = self.plugged {
            let on_cable = match p.device.role() {
                lehu_usb::Role::Apple => {
                    !d.usb_serial.is_empty()
                        && self.plugged_serial.as_deref() == Some(&d.usb_serial)
                }
                lehu_usb::Role::Accessory | lehu_usb::Role::Other => {
                    d.kind == devices::Kind::AndroidAuto && d.bt_address.is_empty()
                }
                lehu_usb::Role::Hub => false,
            };
            if on_cable {
                log::info!("devices: {name} is on the cable");
                match p.device.role() {
                    lehu_usb::Role::Apple => self.start_carplay(p.device),
                    lehu_usb::Role::Accessory => self.start_android(p.device),
                    _ => self.switch_android(p.device),
                }
                self.set_status();
                return;
            }
        }
        self.restart_at = None;
        self.start_wireless();
        match (&self.bt, lehu_bt::Address::parse(&d.bt_address)) {
            (Some(_), Some(_)) => {
                let now = clock::now_ms();
                self.switching = Some(Pending {
                    id: id.to_string(),
                    name,
                    until: now + SWITCH_WAIT_MS,
                    next_try: now + FIRST_TRY_MS,
                });
            }
            _ => {
                log::info!("devices: {name} is known by its cable only");
                self.note = Some((format!("Plug in {name}"), clock::now_ms() + NOTE_MS));
            }
        }
        self.set_status();
    }

    /// The question the screens show, if any: (its generation, the phone
    /// that came, the one in session).
    pub fn switch_question(&self) -> (u32, Option<(&str, &str)>) {
        let current = self
            .active
            .as_ref()
            .and_then(|s| s.phone())
            .unwrap_or("The phone");
        (
            self.question_gen,
            self.question.as_ref().map(|q| (q.name.as_str(), current)),
        )
    }

    /// The answer to "Switch to ...?".
    pub fn answer_switch(&mut self, yes: bool) {
        let Some(q) = self.question.clone() else {
            return;
        };
        if yes {
            self.connect_device(&q.id);
        } else {
            self.withdraw_question(true);
        }
    }

    /// The question goes; the phone's video comes back if it brought the
    /// screens up and `resume` says so.
    fn withdraw_question(&mut self, resume: bool) {
        if self.question.take().is_none() {
            return;
        }
        self.question_gen = self.question_gen.wrapping_add(1);
        if std::mem::take(&mut self.question_backgrounded) && resume {
            self.resume();
        }
    }

    /// A phone that came by itself while another is in session: asked
    /// about, when it is one the head unit knows and not held.
    fn ask_about(&mut self, address: lehu_bt::Address) {
        let addr = address.to_string();
        let Some(d) = self.registry.by_bt(&addr) else {
            return;
        };
        let session_phone = self.active.as_ref().and_then(|s| s.bt_address());
        if d.held
            || self.active_identity().as_deref() == Some(d.id.as_str())
            || session_phone.is_some_and(|a| a.eq_ignore_ascii_case(&addr))
            || self.question.is_some()
            || self.switching.is_some()
        {
            return;
        }
        let name = if d.name.is_empty() {
            "a phone".to_string()
        } else {
            d.name.clone()
        };
        log::info!("devices: {name} came while a session runs; asking");
        self.question = Some(Pending {
            id: d.id.clone(),
            name,
            until: clock::now_ms() + QUESTION_MS,
            next_try: 0,
        });
        self.question_gen = self.question_gen.wrapping_add(1);
        // The question is on the screens: they come over the phone's video.
        if self.front == Front::Phone {
            self.background();
            self.question_backgrounded = true;
        }
    }

    /// A held phone's session reached us anyway (one whose Bluetooth
    /// address was not known yet): it ends at once, and with the address
    /// it gave now known, it is blocked.
    fn refuse_held(&mut self) {
        let Some(id) = self.active_identity() else {
            return;
        };
        if !self.registry.is_held(&id) {
            return;
        }
        self.hold_current();
        self.end_session("held");
        self.restart_at = Some(clock::now_ms() + RESTART_AFTER_MS);
    }

    /// Record the session's phone once it says who it is; the name and
    /// the link's identity are added as they come.
    fn record_phone(&mut self) {
        let Some(s) = &self.active else { return };
        let Some(id) = s.identity() else { return };
        let kind = match s.kind() {
            Kind::CarPlay => devices::Kind::CarPlay,
            Kind::AndroidAuto => devices::Kind::AndroidAuto,
        };
        let name = s.phone().unwrap_or("").to_string();
        let bt = match (s.link(), s.bt_address(), self.bt_phones.as_slice()) {
            (Link::Wifi, Some(address), _) => address,
            (Link::Wifi, None, [one]) => one.to_string(),
            _ => String::new(),
        };
        let usb = if s.link() == Link::Usb {
            self.cp_serial.clone().unwrap_or_default()
        } else {
            String::new()
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        self.registry.seen(&id, kind, &name, &bt, &usb, now);
    }

    /// What the status file says of the session.
    pub fn status_fields(&self) -> crate::status::Fields {
        let (protocol, link) = match &self.active {
            Some(s) if self.phase.alive() => (
                match s.kind() {
                    Kind::CarPlay => "carplay",
                    Kind::AndroidAuto => "android-auto",
                },
                match s.link() {
                    Link::Usb => "usb",
                    Link::Wifi => "wifi",
                },
            ),
            _ => ("", ""),
        };
        let state = match self.phase {
            _ if !self.active() => 1,
            Phase::Spawned => 2,
            Phase::Identified | Phase::Ready => 4,
            Phase::Live | Phase::Backgrounded => 5,
            Phase::Closing | Phase::Closed => 1,
        };
        crate::status::Fields {
            state,
            status: self.status.clone(),
            phone_name: self
                .active
                .as_ref()
                .and_then(|s| s.phone())
                .unwrap_or("")
                .to_string(),
            protocol: protocol.into(),
            link: link.into(),
            video_focused: self.phone_front(),
        }
    }

    /// The nearest moment the driver wants a turn without an event.
    pub fn next_due(&self) -> Option<Duration> {
        let now = clock::now_ms();
        [
            self.restart_at,
            self.unplugged_at,
            self.switching.as_ref().map(|p| p.until),
            self.idle_since
                .filter(|_| self.registry.all().iter().any(|d| d.held))
                .map(|t| t + HOLD_IDLE_MS),
            self.switching
                .as_ref()
                .map(|p| p.next_try)
                .filter(|&t| t != 0),
            self.question.as_ref().map(|p| p.until),
            self.note.as_ref().map(|n| n.1),
        ]
        .into_iter()
        .flatten()
        .min()
        .map(|at| Duration::from_millis(at.saturating_sub(now)))
    }

    /// Everything the session reported since the last turn, and a due
    /// restart on a phone still plugged in.
    pub fn drain(&mut self) {
        self.drain_waiters();
        if self.wants_keyframe.swap(false, Ordering::AcqRel) {
            self.command(Cmd::Keyframe);
        }
        while let Some(event) = self.ports.events.pop() {
            self.on_event(event);
        }
        self.refuse_held();
        self.recover_video();
        // The phone may open Ultra's vehicle state after its picture came.
        let ultra = self.active.as_ref().is_some_and(|s| s.ultra());
        if ultra != self.shown_ultra {
            self.shown_ultra = ultra;
            self.set_status();
        }
        let now = clock::now_ms();
        let no_phone_yet = self.active_identity().is_none();
        if let Some(p) = &mut self.switching
            && no_phone_yet
            && p.next_try != 0
            && now >= p.next_try
            && now < p.until
        {
            p.next_try = now + RETRY_MS;
            let address = self
                .registry
                .by_id(&p.id)
                .and_then(|d| lehu_bt::Address::parse(&d.bt_address));
            if let (Some(bt), Some(address)) = (&self.bt, address) {
                log::info!("devices: connecting {} over Bluetooth", p.name);
                bt.connect_phone(address);
            }
        }
        if let Some(p) = &self.switching {
            if self.active_identity().is_some() {
                // A phone came: the one picked or another, the wait is over.
                self.switching = None;
                self.set_status();
            } else if now >= p.until {
                log::info!("devices: {} did not connect", p.name);
                self.note = Some((format!("{} did not connect", p.name), now + NOTE_MS));
                self.switching = None;
                self.set_status();
            }
        }
        if self.question.as_ref().is_some_and(|q| now >= q.until) {
            self.withdraw_question(true);
        }
        if self.note.as_ref().is_some_and(|n| now >= n.1) {
            self.note = None;
            self.set_status();
        }
        self.release_when_idle(now);
        if let Err(e) = self.registry.save_if_dirty() {
            log::warn!("devices: {e}");
        }
        self.retry_cable(clock::now_ms());
        if self.unplugged_at.is_some_and(|at| clock::now_ms() >= at) {
            self.unplugged_at = None;
            if self.active() && self.link() == Some(Link::Usb) {
                self.end_session("the cable was pulled");
                self.restart_at = Some(clock::now_ms());
            }
        }
        if self.restart_at.is_some_and(|at| clock::now_ms() >= at) {
            self.restart_at = None;
            if !self.active() {
                match self.plugged {
                    Some(p) if p.device.role() == lehu_usb::Role::Apple => {
                        log::info!("session: the phone is still plugged in, starting over");
                        self.start_carplay(p.device);
                    }
                    Some(p) if p.device.role() == lehu_usb::Role::Accessory && !self.aa_hold => {
                        log::info!("session: the accessory is still plugged in, starting on it");
                        self.start_android(p.device);
                    }
                    // Otherwise the wireless listeners wait for phones.
                    _ => self.start_wireless(),
                }
            }
        }
        if self.ports.events.drops() > 0 {
            log::warn!("session: {} events were dropped", self.ports.events.drops());
        }
    }

    fn on_event(&mut self, event: Event) {
        log::debug!("session: {event:?}");
        match event {
            Event::Phase(phase) => {
                self.phase = phase;
                if phase < Phase::Closing {
                    self.reached = self.reached.max(phase);
                }
                if phase >= Phase::Identified && phase < Phase::Closing {
                    self.record_phone();
                    if !self.announced {
                        self.announced = true;
                        run_script(&self.on_connect, "connect");
                    }
                }
                match phase {
                    Phase::Live if self.front == Front::Screens && self.restore.is_none() => {
                        self.front = Front::Phone;
                    }
                    Phase::Closing | Phase::Closed => {
                        self.front = Front::Screens;
                        self.restore = None;
                    }
                    _ => {}
                }
                if phase == Phase::Closed {
                    self.ultra_fallback();
                    self.registry.session_ended();
                    if std::mem::take(&mut self.announced) {
                        run_script(&self.on_disconnect, "disconnect");
                    }
                    self.phone_had_screen = false;
                    self.end_video();
                    if let Some(s) = self.active.take() {
                        s.stop();
                    }
                    self.audio.clear();
                    self.clear_side();
                    // A session that ended with its phone still here (a lost
                    // control connection, the USB reset) is started over, and
                    // so is the wireless one that waits for phones; neither
                    // does that itself.
                    if self.plugged.is_some() || self.wifi.is_some() {
                        self.restart_at = Some(clock::now_ms() + RESTART_AFTER_MS);
                    }
                }
            }
            Event::VideoCodec(codec) => {
                self.codec = Some(codec);
                if self.parked {
                    log::info!("session: {} video; the camera has the memory", codec.name());
                } else {
                    self.start_video(codec);
                }
            }
            Event::VideoFocus(wanted) => {
                self.front = if wanted { Front::Phone } else { Front::Screens };
                self.restore = None;
            }
            Event::HostUi => {
                self.front = Front::Screens;
                self.restore = None;
                self.command(Cmd::VideoActive(false));
            }
            Event::Call(on) | Event::Assistant(on) => {
                // A call or the assistant brings the phone forward; what was
                // in front comes back afterwards.
                if on {
                    if self.front == Front::Screens && self.restore.is_none() {
                        self.restore = Some(Front::Screens);
                        self.front = Front::Phone;
                        self.command(Cmd::VideoActive(true));
                    }
                } else if let Some(back) = self.restore.take() {
                    self.front = back;
                    if back == Front::Screens {
                        self.command(Cmd::VideoActive(false));
                    }
                }
            }
            Event::AudioFlush => self.audio.clear_media(),
            Event::Clock {
                unix_ms,
                zone_offset_min,
                dst_offset_min,
            } => {
                self.zone_min = Some(i32::from(zone_offset_min) + i32::from(dst_offset_min));
                self.phone_clock(unix_ms, zone_offset_min, dst_offset_min);
            }
            Event::NowPlaying | Event::Guidance => self.read_side(),
            Event::Audio(..) | Event::NavFocus(_) => {}
            Event::Fault(fault) => match fault {
                Fault::BadMessage => log::warn!("session: a message made no sense"),
                other => log::warn!("session: {other:?}"),
            },
        }
        self.set_status();
    }

    fn start_video(&mut self, codec: Codec) {
        let Some(display) = self.display.clone() else {
            log::info!("session: {} video, no display to show it", codec.name());
            return;
        };
        // Only the old decoder goes: what the phone already queued stays.
        // Its first units carry the parameter sets every later frame needs,
        // and a phone that has just connected sends them once; flushed
        // here, Android Auto's stream would stay undecodable for good.
        self.stop_decoder();
        let flag = Arc::clone(&self.wants_keyframe);
        let wake = Arc::clone(&self.wake);
        // CarPlay answers a keyframe request; Android Auto has none, so its
        // decoder decodes through a gap instead of holding for a keyframe.
        let keyframe: Option<lehu_video::KeyframeRequest> = (self.kind() == Some(Kind::CarPlay))
            .then(|| {
                Box::new(move || {
                    flag.store(true, Ordering::Release);
                    wake.wake();
                }) as lehu_video::KeyframeRequest
            });
        let options = Options {
            low_delay: self.low_delay,
            fast: self.fast,
            keyframe,
            recycle: Some(Arc::clone(&self.ports.recycle)),
        };
        match Decoder::start(codec, display, Arc::clone(&self.ports.video), options) {
            Ok(d) => {
                self.decoder = Some(d);
                // A phone that streamed to us a moment ago (the cable just
                // pulled) carries its encoder on without a keyframe; the
                // fresh decoder needs one to start from. (Android Auto's
                // only way to one restarts its stream: kept for a decoder
                // that failed, see `recover_video`.)
                if self.kind() == Some(Kind::CarPlay) {
                    self.command(Cmd::Keyframe);
                }
            }
            Err(e) => log::error!("session: {} decoder: {e}", codec.name()),
        }
    }

    /// The decoder goes with the session's video: its cedrus pool is most
    /// of this board's CMA, and the next session needs it back.
    /// The decoder gave up (a stream it could not decode, a run of frames
    /// without their parameter sets): a fresh one is started on the same
    /// codec and the phone asked for a keyframe, which for Android Auto
    /// restarts its stream with the parameter sets in front. At most once
    /// every few seconds, so a stream the hardware cannot take does not
    /// spin.
    fn recover_video(&mut self) {
        let (Some(d), Some(codec)) = (&self.decoder, self.codec) else {
            return;
        };
        if !d.failed() || self.parked {
            return;
        }
        let now = clock::now_ms();
        if now.saturating_sub(self.recovered_at) < RECOVER_EVERY_MS {
            return;
        }
        self.recovered_at = now;
        log::warn!("session: the decoder gave up; starting it again and asking for a keyframe");
        self.start_video(codec);
        self.command(Cmd::Keyframe);
    }

    /// The decoder stops; the queue is left as it is.
    fn stop_decoder(&mut self) {
        if let Some(d) = self.decoder.take() {
            let st = d.stats();
            log::info!(
                "session: decoder stopped, {} decoded, {} shown, {} dropped",
                st.decoded,
                st.shown,
                st.dropped
            );
            d.stop();
        }
    }

    fn end_video(&mut self) {
        if let Some(d) = self.decoder.take() {
            let st = d.stats();
            log::info!(
                "session: video ends, {} decoded, {} shown, {} dropped",
                st.decoded,
                st.shown,
                st.dropped
            );
            d.stop();
        }
        self.ports.video.clear();
        // The spare units hold up to a keyframe each: not kept while the
        // camera shows or no phone is there; the next video makes new ones.
        self.ports.recycle.clear();
        crate::mem::trim();
    }

    /// The phone's wall clock: the system's is set by it when far off (a
    /// head unit has no battery-backed clock), through the image's
    /// command, which has the privilege this program does not.
    fn phone_clock(&self, unix_ms: i64, zone_offset_min: i16, dst_offset_min: i8) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let off = unix_ms - now_ms;
        let zone = i32::from(zone_offset_min) + i32::from(dst_offset_min);
        log::info!(
            "session: the phone's clock is {} s from ours, zone UTC{}{:02}:{:02}",
            off / 1000,
            if zone < 0 { '-' } else { '+' },
            zone.abs() / 60,
            zone.abs() % 60
        );
        if off.abs() < CLOCK_SLACK_MS || self.set_clock.is_empty() {
            return;
        }
        let command = self
            .set_clock
            .replace("{unix}", &(unix_ms / 1000).to_string())
            .replace("{zone}", &zone.to_string());
        spawn_waited(lehu_core::shell::privileged(&command), "set-clock");
    }

    /// A flip completed: the decoder may show the frame it had waiting.
    pub fn on_flip(&self) {
        if let Some(d) = &self.decoder {
            d.on_flip();
        }
    }

    pub fn command(&self, cmd: Cmd) {
        if let Some(s) = &self.active {
            s.command(cmd);
        }
    }

    /// The Menu button with the phone in front: the screens come up and the
    /// phone is told it lost the panel.
    pub fn background(&mut self) {
        if self.front == Front::Phone {
            self.front = Front::Screens;
            self.restore = None;
            self.command(Cmd::VideoActive(false));
        }
    }

    /// Back to the phone from the screens, when there is one to go to.
    pub fn resume(&mut self) {
        if self.active() && self.phase >= Phase::Ready && self.front == Front::Screens {
            self.front = Front::Phone;
            self.restore = None;
            self.command(Cmd::Resume);
            self.command(Cmd::VideoActive(true));
        }
    }

    pub fn touch(&self, count: u8, contacts: [Contact; MAX_SLOTS]) {
        self.command(Cmd::Touch { count, contacts });
    }

    pub fn button(&mut self, button: PhoneButton, value: Value) {
        match (button, value) {
            (PhoneButton::Menu, Value::Press) => self.background(),
            (PhoneButton::Menu, _) => {}
            (PhoneButton::RotateNext, Value::Press) => self.command(Cmd::Rotary(1)),
            (PhoneButton::RotatePrev, Value::Press) => self.command(Cmd::Rotary(-1)),
            (PhoneButton::RotateNext | PhoneButton::RotatePrev, _) => {}
            (b, Value::Press | Value::Repeat) => self.command(Cmd::Key {
                button: b,
                down: true,
            }),
            (b, Value::Release) => self.command(Cmd::Key {
                button: b,
                down: false,
            }),
        }
    }

    /// Shutdown: the phone is told goodbye, the decoder stopped.
    pub fn stop(&mut self) {
        self.restart_at = None;
        self.end_waiters("stops");
        if std::mem::take(&mut self.announced) {
            run_script(&self.on_disconnect, "disconnect");
        }
        self.end_video();
        if let Some(s) = self.active.take() {
            s.stop();
        }
        self.audio.clear();
        self.clear_side();
        self.phase = Phase::Closed;
        self.front = Front::Screens;
    }
}

/// A user's hook, through the shell, waited for on a thread of its own so
/// it never holds the loop and leaves no zombie.
fn run_script(command: &str, what: &str) {
    if command.is_empty() {
        return;
    }
    log::info!("session: {what} script: {command}");
    spawn_waited(lehu_core::shell::shell(command), what);
}

fn spawn_waited(mut command: std::process::Command, what: &str) {
    match command.spawn() {
        Ok(mut child) => {
            std::thread::Builder::new()
                .name("script".into())
                .spawn(move || {
                    let _ = child.wait();
                })
                .ok();
        }
        Err(e) => log::warn!("session: {what} script: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    struct Fake {
        cmds: Arc<Mutex<Vec<Cmd>>>,
        stops: Arc<AtomicUsize>,
        identity: Option<String>,
        bt: Option<String>,
        link: Link,
        ultra: Arc<AtomicBool>,
        kind: Kind,
    }

    impl Fake {
        fn new(cmds: &Arc<Mutex<Vec<Cmd>>>, stops: &Arc<AtomicUsize>) -> Self {
            Self {
                cmds: Arc::clone(cmds),
                stops: Arc::clone(stops),
                identity: None,
                bt: None,
                link: Link::Usb,
                ultra: Arc::new(AtomicBool::new(false)),
                kind: Kind::CarPlay,
            }
        }
    }

    impl Session for Fake {
        fn kind(&self) -> Kind {
            self.kind
        }
        fn link(&self) -> Link {
            self.link
        }
        fn identity(&self) -> Option<String> {
            self.identity.clone()
        }
        fn bt_address(&self) -> Option<String> {
            self.bt.clone()
        }
        fn ultra(&self) -> bool {
            self.ultra.load(Ordering::SeqCst)
        }
        fn phase(&self) -> Phase {
            Phase::Spawned
        }
        fn phone(&self) -> Option<&str> {
            Some("Test")
        }
        fn command(&self, cmd: Cmd) {
            self.cmds.lock().unwrap().push(cmd);
        }
        fn stop(self: Box<Self>) {
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn test_driver() -> Driver {
        let wake = Arc::new(Wake::new().unwrap());
        let settings = Settings::default();
        let audio = Arc::new(Audio::start(&settings.audio));
        let mut d = Driver::new(wake, None, &settings, audio, None, std::env::temp_dir());
        d.registry = Registry::in_memory();
        d.wifi = None; // no listeners to start in the tests
        d.restart_at = None;
        d
    }

    fn driver() -> (Driver, Arc<Mutex<Vec<Cmd>>>, Arc<AtomicUsize>) {
        let mut d = test_driver();
        let cmds = Arc::new(Mutex::new(Vec::new()));
        let stops = Arc::new(AtomicUsize::new(0));
        d.adopt(Box::new(Fake::new(&cmds, &stops)));
        (d, cmds, stops)
    }

    const A_BT: &str = "11:11:11:11:11:11";
    const B_BT: &str = "22:22:22:22:22:22";

    /// A wireless session on phone `id` that said who it is and is live.
    fn live(
        d: &mut Driver,
        id: &str,
        bt: Option<&str>,
    ) -> (Arc<Mutex<Vec<Cmd>>>, Arc<AtomicUsize>) {
        let cmds = Arc::new(Mutex::new(Vec::new()));
        let stops = Arc::new(AtomicUsize::new(0));
        let mut fake = Fake::new(&cmds, &stops);
        fake.identity = Some(id.into());
        fake.bt = bt.map(str::to_string);
        fake.link = Link::Wifi;
        d.adopt(Box::new(fake));
        d.ports.report(Event::Phase(Phase::Identified));
        d.ports.report(Event::Phase(Phase::Live));
        d.drain();
        (cmds, stops)
    }

    fn address(a: &str) -> lehu_bt::Address {
        lehu_bt::Address::parse(a).unwrap()
    }

    #[test]
    fn the_header_names_carplay_ultra_once_the_phone_runs_it() {
        let mut d = test_driver();
        let cmds = Arc::new(Mutex::new(Vec::new()));
        let stops = Arc::new(AtomicUsize::new(0));
        let fake = Fake::new(&cmds, &stops);
        let ultra = Arc::clone(&fake.ultra);
        d.adopt(Box::new(fake));
        d.ports.report(Event::Phase(Phase::Live));
        d.drain();
        assert_eq!(d.take_status(), Some("CarPlay: Test"));
        // The vehicle state opened after the picture came.
        ultra.store(true, Ordering::SeqCst);
        d.drain();
        assert_eq!(d.take_status(), Some("CarPlay Ultra: Test"));
        d.drain();
        assert_eq!(d.take_status(), None, "said once");
    }

    #[test]
    fn an_ultra_session_that_never_goes_live_falls_back_to_classic() {
        let ultra = |d: &Driver| d.carplay.as_ref().is_some_and(|c| c.ultra);
        // A session that went live keeps Ultra.
        let mut d = test_driver();
        assert!(ultra(&d), "on by default");
        let _ = live(&mut d, "a", Some(A_BT));
        d.ports.report(Event::Phase(Phase::Closed));
        d.drain();
        assert!(ultra(&d));
        // One that never met its phone is no verdict on Ultra.
        let (mut d, _, _) = driver();
        d.ports.report(Event::Phase(Phase::Closed));
        d.drain();
        assert!(ultra(&d));
        // One that met its phone and ended before the picture: classic.
        let (mut d, _, _) = driver();
        d.ports.report(Event::Phase(Phase::Identified));
        d.ports.report(Event::Phase(Phase::Closing));
        d.ports.report(Event::Phase(Phase::Closed));
        d.drain();
        assert!(!ultra(&d));
    }

    #[test]
    fn stop_holds_the_phone_and_a_held_phone_is_refused() {
        let mut d = test_driver();
        let (_, stops) = live(&mut d, "a", Some(A_BT));
        assert_eq!(d.registry.by_id("a").unwrap().bt_address, A_BT);
        d.stop_device("b"); // not the phone in session: nothing
        assert!(d.active());
        d.stop_device("a");
        assert!(!d.active());
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert!(d.registry.is_held("a"));
        assert!(d.restart_at.is_some(), "the listeners open again");
        // The held phone comes back by itself: refused at once.
        let (_, stops) = live(&mut d, "a", Some(A_BT));
        assert!(!d.active());
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert!(d.registry.is_held("a"));
        // Connect lets it back.
        d.connect_device("a");
        assert!(!d.registry.is_held("a"));
    }

    #[test]
    fn connect_on_another_phone_stops_and_holds_the_one_in_session() {
        let mut d = test_driver();
        d.registry
            .seen("b", devices::Kind::AndroidAuto, "Pixel", B_BT, "", 1);
        let (_, stops) = live(&mut d, "a", Some(A_BT));
        d.connect_device("a"); // the phone in session: nothing
        assert!(d.active());
        d.connect_device("b");
        assert!(!d.active());
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert!(d.registry.is_held("a"));
        assert!(!d.registry.is_held("b"));
    }

    #[test]
    fn holds_end_when_no_phone_is_in_session() {
        let mut d = test_driver();
        let _ = live(&mut d, "a", Some(A_BT));
        d.stop_device("a");
        assert!(d.registry.is_held("a"));
        let t0 = clock::now_ms();
        d.release_when_idle(t0);
        d.release_when_idle(t0 + HOLD_IDLE_MS - 1);
        assert!(
            d.registry.is_held("a"),
            "the moment after Stop is protected"
        );
        d.release_when_idle(t0 + HOLD_IDLE_MS);
        assert!(!d.registry.is_held("a"));
        // A session running keeps the hold.
        let _ = live(&mut d, "a", Some(A_BT));
        d.connect_device("b"); // unknown: nothing
        d.registry
            .seen("b", devices::Kind::CarPlay, "B", B_BT, "", 1);
        let _ = live(&mut d, "a", Some(A_BT));
        d.connect_device("b");
        assert!(d.registry.is_held("a"));
        let _ = live(&mut d, "b", Some(B_BT));
        d.release_when_idle(t0 + 10 * HOLD_IDLE_MS);
        assert!(
            d.registry.is_held("a"),
            "held while another phone is in session"
        );
    }

    #[test]
    fn a_known_phone_coming_is_asked_about() {
        let mut d = test_driver();
        d.registry
            .seen("b", devices::Kind::CarPlay, "Pixel", B_BT, "", 1);
        let (cmds, stops) = live(&mut d, "a", Some(A_BT));
        assert_eq!(d.front(), Front::Phone);
        // The session's own phone, or a stranger: no question.
        d.bt_phone(address(A_BT), true);
        d.bt_phone(address("33:33:33:33:33:33"), true);
        assert_eq!(d.switch_question().1, None);
        // A known phone: asked, over the screens.
        let generation = d.switch_question().0;
        d.bt_phone(address(B_BT), true);
        assert_eq!(d.switch_question().1, Some(("Pixel", "Test")));
        assert_ne!(d.switch_question().0, generation);
        assert_eq!(d.front(), Front::Screens);
        // No: the question goes, the phone's video comes back.
        d.answer_switch(false);
        assert_eq!(d.switch_question().1, None);
        assert_eq!(d.front(), Front::Phone);
        assert_eq!(cmds.lock().unwrap().last(), Some(&Cmd::VideoActive(true)));
        // It leaves while asked: the question goes with it.
        d.bt_phone(address(B_BT), false);
        d.bt_phone(address(B_BT), true);
        assert!(d.switch_question().1.is_some());
        d.bt_phone(address(B_BT), false);
        assert_eq!(d.switch_question().1, None);
        // Yes: the session ends and its phone is held.
        d.bt_phone(address(B_BT), true);
        d.answer_switch(true);
        assert_eq!(d.switch_question().1, None);
        assert!(!d.active());
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert!(d.registry.is_held("a"));
        // A held phone is never asked about.
        let _ = live(&mut d, "b", Some(B_BT));
        d.bt_phone(address(A_BT), true);
        assert_eq!(d.switch_question().1, None);
    }

    #[test]
    fn live_brings_the_phone_forward_and_menu_takes_it_back() {
        let (mut d, cmds, _) = driver();
        assert_eq!(d.front(), Front::Screens);
        assert!(!d.takes_buttons());
        d.ports.report(Event::VideoCodec(Codec::H264));
        d.ports.report(Event::Phase(Phase::Live));
        d.drain();
        assert_eq!(d.front(), Front::Phone);
        assert!(d.takes_buttons());
        assert_eq!(d.take_status(), Some("CarPlay: Test"));
        assert_eq!(d.take_status(), None);
        d.button(PhoneButton::Menu, Value::Press);
        assert_eq!(d.front(), Front::Screens);
        assert_eq!(cmds.lock().unwrap().last(), Some(&Cmd::VideoActive(false)));
        d.resume();
        assert_eq!(d.front(), Front::Phone);
        let got = cmds.lock().unwrap().clone();
        assert_eq!(
            &got[got.len() - 2..],
            &[Cmd::Resume, Cmd::VideoActive(true)]
        );
    }

    #[test]
    fn a_call_comes_forward_and_the_screens_return() {
        let (mut d, cmds, _) = driver();
        d.ports.report(Event::Phase(Phase::Live));
        d.drain();
        d.background();
        d.ports.report(Event::Call(true));
        d.drain();
        assert_eq!(d.front(), Front::Phone);
        d.ports.report(Event::Call(false));
        d.drain();
        assert_eq!(d.front(), Front::Screens);
        assert_eq!(cmds.lock().unwrap().last(), Some(&Cmd::VideoActive(false)));
    }

    #[test]
    fn closed_drops_the_session_and_adopt_supersedes() {
        let (mut d, _, stops) = driver();
        d.ports.report(Event::Phase(Phase::Closed));
        d.drain();
        assert!(!d.active());
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert_eq!(d.take_status(), Some("No phone"));
        let (mut d, _, stops) = driver();
        d.adopt(Box::new(Fake::new(
            &Arc::new(Mutex::new(Vec::new())),
            &stops,
        )));
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        d.stop();
        assert_eq!(stops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_camera_parks_the_phone_and_gives_it_back() {
        let (mut d, cmds, _) = driver();
        d.ports.report(Event::VideoCodec(Codec::H264));
        d.ports.report(Event::Phase(Phase::Live));
        d.drain();
        assert!(d.phone_front());
        d.park(true);
        assert!(d.phone_had_screen());
        assert_eq!(cmds.lock().unwrap().last(), Some(&Cmd::VideoActive(false)));
        // A codec reported while parked is remembered, not started.
        d.ports.report(Event::VideoCodec(Codec::H264));
        d.drain();
        assert!(!d.video_running());
        d.unpark();
        assert!(!d.phone_had_screen());
        let got = cmds.lock().unwrap().clone();
        assert_eq!(
            &got[got.len() - 2..],
            &[Cmd::VideoActive(true), Cmd::Keyframe]
        );
        // Parked without the decoder (fast switch): only the phone is told.
        d.park(false);
        d.unpark();
        assert_eq!(cmds.lock().unwrap().len(), got.len() + 3);
    }

    #[test]
    fn the_session_reports_into_the_side_channel_and_its_end_clears_it() {
        let (mut d, cmds, _) = driver();
        assert_eq!(d.now_playing().0, 0);
        d.ports.tell_now_playing(|np| {
            lehu_session::set_text(&mut np.title, "Song");
            true
        });
        d.ports.tell_guidance(|g| {
            g.active = true;
            g.distance_m = Some(120);
            true
        });
        d.ports.report(Event::Clock {
            unix_ms: 0,
            zone_offset_min: 60,
            dst_offset_min: 60,
        });
        d.drain();
        let (np_gen, np) = d.now_playing();
        assert_eq!((np_gen, np.title.as_str()), (1, "Song"));
        let (g_gen, g) = d.guidance();
        assert_eq!((g_gen, g.distance_m), (1, Some(120)));
        assert_eq!(d.zone_min(), Some(120));
        // Nothing new, nothing moves.
        d.drain();
        assert_eq!(d.now_playing().0, 1);
        // The Media page's keys are whole presses.
        d.media_key(PhoneButton::NextTrack);
        assert_eq!(
            cmds.lock().unwrap()[..],
            [
                Cmd::Key {
                    button: PhoneButton::NextTrack,
                    down: true
                },
                Cmd::Key {
                    button: PhoneButton::NextTrack,
                    down: false
                }
            ]
        );
        // The album art: decoded off the loop, at the page's size.
        let jpeg = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../crates/lehu-image/fixtures/base420.jpg"
        ))
        .unwrap();
        d.ports.tell_artwork(|a| a.set(&jpeg));
        d.drain();
        let art = (0..400)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(5));
                d.take_art()
            })
            .expect("the art, decoded");
        assert_eq!(art.size(), (lehu_ui::ART_SIZE, 94));
        // The session ends: all go, and say so.
        d.ports.report(Event::Phase(Phase::Closed));
        d.drain();
        assert_eq!(d.now_playing(), (2, &NowPlaying::default()));
        assert_eq!(d.guidance(), (2, &Guidance::default()));
        assert!(d.take_art().expect("no art now").is_none());
        // A new session starts from its own side channel.
        let cmds = Arc::new(Mutex::new(Vec::new()));
        let stops = Arc::new(AtomicUsize::new(0));
        let ports = d.fresh_ports();
        d.adopt(Box::new(Fake::new(&cmds, &stops)));
        ports.tell_now_playing(|np| {
            lehu_session::set_text(&mut np.title, "Other");
            true
        });
        d.drain();
        assert_eq!(d.now_playing().1.title, "Other");
        assert_eq!(d.now_playing().0, 3);
    }

    #[test]
    fn buttons_become_commands() {
        let (mut d, cmds, _) = driver();
        d.button(PhoneButton::Select, Value::Press);
        d.button(PhoneButton::Select, Value::Release);
        d.button(PhoneButton::RotateNext, Value::Press);
        d.button(PhoneButton::RotateNext, Value::Release);
        let got = cmds.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                Cmd::Key {
                    button: PhoneButton::Select,
                    down: true
                },
                Cmd::Key {
                    button: PhoneButton::Select,
                    down: false
                },
                Cmd::Rotary(1),
            ]
        );
    }

    /// An Android phone already in accessory mode.
    fn android_phone() -> lehu_usb::Device {
        lehu_usb::Device::new(
            1,
            3,
            lehu_usb::Id {
                vendor: 0x18d1,
                product: 0x2d00,
            },
            0,
            "1-1",
        )
    }

    #[test]
    fn a_cable_we_cannot_open_leaves_the_wireless_session_until_it_is_ours() {
        let mut d = test_driver();
        d.node_writable = |_| false;
        d.switched = true; // switched by us, not a left-over of an earlier run
        let (_, stops) = live(&mut d, "phone-a", Some(A_BT));
        let phone = android_phone();
        d.usb(lehu_usb::Event::Attached(phone));
        assert_eq!(stops.load(Ordering::SeqCst), 0);
        assert!(d.active());
        assert_eq!(d.usb_wait.map(|(dev, _)| dev), Some(phone));
        // Still not ours a turn later: nothing changes.
        d.drain();
        assert_eq!(stops.load(Ordering::SeqCst), 0);
        // udev gives it to us: the cable takes over as it always did.
        d.node_writable = |_| true;
        d.drain();
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert!(d.usb_wait.is_none());
    }

    #[test]
    fn a_cable_that_never_becomes_ours_is_given_up_and_the_session_stays() {
        let mut d = test_driver();
        d.node_writable = |_| false;
        let (_, stops) = live(&mut d, "phone-a", Some(A_BT));
        d.usb(lehu_usb::Event::Attached(android_phone()));
        d.usb_wait = d.usb_wait.map(|(dev, _)| (dev, 0));
        d.drain();
        assert!(d.usb_wait.is_none());
        assert_eq!(stops.load(Ordering::SeqCst), 0);
        assert!(d.active());
    }

    #[test]
    fn a_new_frame_rate_starts_a_running_android_auto_session_again() {
        let mut d = test_driver();
        let old = d.android.offer.fps;
        // Unchanged: nothing happens.
        d.set_android_fps(old);
        assert!(d.restart_at.is_none());
        let cmds = Arc::new(Mutex::new(Vec::new()));
        let stops = Arc::new(AtomicUsize::new(0));
        let mut fake = Fake::new(&cmds, &stops);
        fake.kind = Kind::AndroidAuto;
        fake.link = Link::Wifi;
        d.adopt(Box::new(fake));
        let fps = if old == 60 { 30 } else { 60 };
        d.set_android_fps(fps);
        assert_eq!(d.android.offer.fps, fps);
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert!(!d.active());
    }

    #[test]
    fn a_new_frame_rate_leaves_a_carplay_session_alone() {
        let (mut d, _, stops) = driver();
        let fps = if d.android.offer.fps == 60 { 30 } else { 60 };
        d.set_android_fps(fps);
        assert_eq!(d.android.offer.fps, fps);
        assert_eq!(stops.load(Ordering::SeqCst), 0);
        assert!(d.active());
    }

    #[test]
    fn usb_shows_in_the_status_until_a_session_runs() {
        let mut d = test_driver();
        assert_eq!(d.take_status(), Some("No phone"));
        let hub = lehu_usb::Device::new(
            1,
            1,
            lehu_usb::Id {
                vendor: 0x1d6b,
                product: 2,
            },
            9,
            "usb1",
        );
        let phone = lehu_usb::Device::new(
            1,
            2,
            lehu_usb::Id {
                vendor: 0x05ac,
                product: 0x12a8,
            },
            0,
            "1-1",
        );
        d.usb(lehu_usb::Event::Attached(hub));
        assert_eq!(d.take_status(), None);
        d.usb(lehu_usb::Event::Attached(phone));
        assert_eq!(d.take_status(), Some("iPhone plugged in"));
        assert_eq!(d.plugged().map(|p| p.device), Some(phone));
        d.usb(lehu_usb::Event::Detached(phone));
        assert_eq!(d.take_status(), Some("No phone"));
        assert_eq!(d.plugged(), None);
    }
}
