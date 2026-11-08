//! What the screens need from the rest of the program, and nothing more:
//! the contract FastCarPlay kept in ui_bridge and in the singletons its
//! controller reached for (settings, volume, camera, Bluetooth). The
//! program implements it once; the controller is tested against a recording
//! stand-in. Every method is called on the loop thread, inside `Osd`'s
//! calls; none may block.

use lehu_core::settings::{self, Settings};
use lehu_wifi::{Applied, Link, Radio};
use toml::Value;

/// What the System rows ask for once their question was answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    Reboot,
    Shutdown,
}

/// A camera switch asked from outside the screens: the camera key today,
/// a reverse signal later.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CameraRequest {
    #[default]
    None,
    Toggle,
    On,
    Off,
}

/// The kind of dialog Bluetooth is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PromptKind {
    #[default]
    None,
    /// A code to type on the device; Cancel is the only button.
    Code,
    /// A yes/no question.
    Question,
}

/// Bluetooth's dialog, as the manager has it right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prompt {
    pub kind: PromptKind,
    pub title: String,
    pub code: String,
    pub hint: String,
    /// The accept button's label (a question only).
    pub accept: String,
}

/// One Bluetooth device as the lists show it. `id` is the BlueZ object
/// path: callbacks carry it, because a row can move under a finger.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BtDevice {
    pub id: String,
    pub name: String,
    pub detail: String,
    pub connected: bool,
}

/// One phone the head unit knows, as the Devices screen lists it. The
/// program builds these from the registry, in the order the screen shows
/// them -- the phone the session runs on, then the ones on a link now,
/// then the rest by last seen -- so the screens never read the registry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KnownDevice {
    /// The registry's id: callbacks carry it, because a row can move.
    pub id: String,
    /// What the phone calls itself; empty shows as "Phone".
    pub name: String,
    /// Where it is and what it speaks: "Connected", "Nearby" or "Not here",
    /// then "CarPlay" or "Android Auto".
    pub detail: String,
    /// The session runs on it.
    pub active: bool,
    /// On a link now (and not the session's): the list's grey dot.
    pub nearby: bool,
}

/// One picture level of the camera (Brightness, Contrast...), with its
/// value as text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CameraLevel {
    pub label: String,
    pub value: String,
}

/// What the phone plays, as the Home row and the Media page show it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Media {
    /// Something to show: the Home row is there.
    pub available: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// 0 when unknown: the position line shows the position alone.
    pub duration_ms: u32,
    /// Where it was at `position_at`, the program's clock
    /// (`lehu_core::clock::now_ms`); it runs on from there while playing.
    pub position_ms: u32,
    pub position_at: u64,
    pub playing: bool,
}

/// The album art's size on the Media page: its longest side, in pixels.
/// The picture is decoded to it once, so the page never scales it.
pub const ART_SIZE: u32 = 160;

/// The album art as the Media page shows it: RGB at its shown size
/// (`ART_SIZE` on its longest side), made off the loop's thread and handed
/// to the screens as it is, no copy. `Art::default()` is no art.
#[derive(Clone, Default)]
pub struct Art {
    pixels: Option<slint::SharedPixelBuffer<slint::Rgb8Pixel>>,
}

impl std::fmt::Debug for Art {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.pixels {
            Some(p) => write!(f, "Art({}x{})", p.width(), p.height()),
            None => f.write_str("Art(none)"),
        }
    }
}

impl Art {
    /// `rgb` is `width` x `height` pixels, three bytes each; a size that
    /// does not match it, or is empty, is no art.
    pub fn from_rgb(width: u32, height: u32, rgb: &[u8]) -> Self {
        if width == 0 || height == 0 || rgb.len() != width as usize * height as usize * 3 {
            return Self::default();
        }
        let mut pixels = slint::SharedPixelBuffer::new(width, height);
        pixels.make_mut_bytes().copy_from_slice(rgb);
        Self {
            pixels: Some(pixels),
        }
    }

    pub fn is_none(&self) -> bool {
        self.pixels.is_none()
    }

    pub fn size(&self) -> (u32, u32) {
        self.pixels
            .as_ref()
            .map_or((0, 0), |p| (p.width(), p.height()))
    }

    /// The picture for the screens; `None` for no art.
    pub(crate) fn into_image(self) -> Option<slint::Image> {
        self.pixels.map(slint::Image::from_rgb8)
    }
}

/// A transport key on the Media page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKey {
    Previous,
    PlayPause,
    Next,
}

/// The arrow the navigation card draws for the next manoeuvre: a few
/// shapes a glance can take, the words saying the rest. Its number is the
/// card's `nav-arrow`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NavArrow {
    /// No arrow: the words alone.
    #[default]
    None,
    Straight,
    SlightLeft,
    Left,
    SharpLeft,
    SlightRight,
    Right,
    SharpRight,
    UTurn,
    Roundabout,
    Arrive,
}

impl NavArrow {
    /// As the screens number it.
    pub fn id(self) -> i32 {
        match self {
            NavArrow::None => 0,
            NavArrow::Straight => 1,
            NavArrow::SlightLeft => 2,
            NavArrow::Left => 3,
            NavArrow::SharpLeft => 4,
            NavArrow::SlightRight => 5,
            NavArrow::Right => 6,
            NavArrow::SharpRight => 7,
            NavArrow::UTurn => 8,
            NavArrow::Roundabout => 9,
            NavArrow::Arrive => 10,
        }
    }
}

/// The navigation card, its words composed by the program.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NavCard {
    /// The phone is guiding: the card shows for a while after each change.
    pub active: bool,
    /// The manoeuvre drawn.
    pub arrow: NavArrow,
    /// The manoeuvre in words ("Turn left").
    pub maneuver: String,
    /// To the manoeuvre ("350 m"), or empty.
    pub distance: String,
    /// The road it leads onto.
    pub road: String,
    /// The arrival ("14:32", "12 min"), or empty.
    pub eta: String,
    /// What `eta` is, under it: "arrival", "to go"; empty with no eta.
    pub eta_note: String,
}

/// An ALSA playback device, as the Output row offers it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioOutput {
    /// The PCM the setting names (`hw:CARD=Codec,DEV=0`).
    pub device: String,
    /// What the card calls itself.
    pub label: String,
    /// `audio.device` opens it.
    pub active: bool,
}

/// Where a LEHU OS update is (Settings > Update).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UpdateStage {
    /// Nothing, or part of one waiting for the phone to send the rest.
    #[default]
    Idle,
    Receiving,
    Checking,
    /// Checked, to install.
    Ready,
    Failed,
    Installing,
    /// The head unit restarts by itself.
    Restarting,
}

/// How the last update went, said once after the restart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UpdateOutcome {
    #[default]
    None,
    /// It runs now: its version.
    Updated(String),
    /// Why it did not go in.
    Failed(String),
    /// Cut off: it can be installed again.
    Interrupted,
}

/// Settings > Update as the program has it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Update {
    /// An update server is set: the Settings row is there.
    pub shown: bool,
    /// This system installs updates (its installer is there).
    pub available: bool,
    /// The version running; empty while not known.
    pub version: String,
    pub stage: UpdateStage,
    /// Bytes of the update received, of its size.
    pub received: u64,
    pub size: u64,
    /// Once checked: its version, the update server's line about it, its
    /// size unpacked.
    pub new_version: String,
    pub notes: String,
    pub size_kb: u64,
    /// It installs through a restart into the update boot (minutes, the
    /// power must stay on), not beside the running system.
    pub restart: bool,
    /// While installing.
    pub percent: u32,
    /// Why it failed, or the install's step.
    pub message: String,
    /// The page for the phone, while it is served: the access point to
    /// join and its address; or why it cannot be.
    pub serving: bool,
    pub ssid: String,
    pub passphrase: String,
    pub url: String,
    pub serve_error: String,
    pub outcome: UpdateOutcome,
    /// There is something to cancel, and stopping now is safe.
    pub can_cancel: bool,
}

/// What the system is, for Settings > System > Info.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemNames {
    pub kernel: String,
    /// The image's own name, when LEHU OS does not give its version.
    pub os: String,
    pub lehu: String,
}

/// The program as the screens see it.
pub trait Host {
    // ── settings ─────────────────────────────────────────────────────────

    /// The settings in force; what the rows show comes from here.
    fn settings(&self) -> &Settings;
    /// Set a dotted key (`display.night-mode`) in the user's file and in the
    /// settings in force at once; the file is saved after the debounce.
    fn persist(&mut self, key: &str, value: Value) -> Result<(), settings::Error>;
    /// The music buffer, applied to the mixer at once but not yet written.
    fn apply_audio_buffer(&mut self, ms: u32);
    /// Write the access point's name and passphrase into hostapd's config
    /// and restart it; on success the settings reflect what it advertises.
    fn configure_access_point(
        &mut self,
        ssid: &str,
        passphrase: &str,
    ) -> Result<Applied, lehu_wifi::Error>;
    /// The access point's radio as hostapd's config has it now (read from
    /// the file: when its page opens, not per tick).
    fn access_point(&mut self) -> Result<Radio, lehu_wifi::Error>;
    /// Write the radio into hostapd's config and restart the AP; refused,
    /// the file untouched, unless hostapd would start with it.
    fn configure_radio(&mut self, radio: &Radio) -> Result<Applied, lehu_wifi::Error>;
    /// The playback devices there are now (listed when the Audio screen
    /// opens).
    fn audio_outputs(&mut self) -> &[AudioOutput];
    /// The phone's Wi-Fi link, sampled now: about once a second while its
    /// page is up.
    fn link(&mut self) -> Link;

    // ── volume ───────────────────────────────────────────────────────────

    /// The level, 0..=100.
    fn volume_level(&self) -> u32;
    /// Muted by a key or a switch (0% counts as muted on the screens too).
    fn volume_muted(&self) -> bool;
    /// Changes with every level or mute change, from any thread.
    fn volume_generation(&self) -> u32;
    /// One step up (+1) or down (-1), applied at once.
    fn volume_step(&mut self, direction: i32);

    // ── session ──────────────────────────────────────────────────────────

    /// The session or what a phone is doing, as one line: "CarPlay: Alex's
    /// iPhone" (the protocol, a colon, the phone) while a session runs,
    /// "iPhone plugged in", "Switching to Pixel..."; "" or "No phone" for
    /// nothing at all.
    fn status(&self) -> &str;
    /// The local time of day for the bar's clock, in milliseconds since
    /// local midnight; `None` while the system clock is not set.
    fn local_time_ms(&self) -> Option<u64>;
    /// A connected session waits behind the screens.
    fn session_backgrounded(&self) -> bool;
    /// The phone's session runs over USB.
    fn link_usb(&self) -> bool;
    /// The phone's session runs over Wi-Fi.
    fn link_wifi(&self) -> bool;
    /// The Resume row (or Back and Menu on the home screen): project again.
    fn request_resume(&mut self);
    /// The Night setting changed; a running session switches at once.
    fn night_mode_changed(&mut self);
    /// Reboot or shut down, once the question was answered.
    fn request_power(&mut self, action: Power);

    // ── camera ───────────────────────────────────────────────────────────

    /// A capture device is there: the Video screen shows its rows.
    fn camera_present(&self) -> bool;
    /// How many inputs the device has.
    fn camera_inputs(&self) -> usize;
    /// The input on screen.
    fn camera_input(&self) -> usize;
    /// The input on screen has no signal.
    fn camera_no_signal(&self) -> bool;
    /// The device has a selectable video standard.
    fn camera_has_standard(&self) -> bool;
    /// The standard as the column shows it ("Auto", "PAL"...).
    fn camera_standard_label(&self) -> &str;
    /// The picture levels the device has, with their values as text.
    fn camera_levels(&self) -> &[CameraLevel];
    /// Changes whenever any of the above does.
    fn camera_generation(&self) -> u32;
    /// Show this input, live.
    fn camera_select_input(&mut self, index: usize);
    /// The next (+1) or previous (-1) standard; the stream restarts.
    fn camera_cycle_standard(&mut self, direction: i32);
    /// One level one step up or down, applied at once.
    fn camera_adjust_level(&mut self, index: usize, direction: i32);
    /// The camera view is up (or gone): the loop runs (or stops) the camera.
    fn set_camera_running(&mut self, on: bool);
    /// The camera could not start or stopped: why, once, into `why`.
    fn take_camera_failure(&mut self, why: &mut String) -> bool;
    /// A switch asked from outside the screens, once.
    fn take_camera_request(&mut self) -> CameraRequest;
    /// While on, the next key on a Bluetooth keyboard is learned, not acted on.
    fn set_key_learning(&mut self, on: bool);
    /// The key that was learned, once (its Linux key code).
    fn take_learned_key(&mut self) -> Option<u32>;

    // ── bluetooth ────────────────────────────────────────────────────────

    /// Built and running with Bluetooth: the Wireless screen shows its row.
    fn bt_available(&self) -> bool;
    /// Changes whenever any of the Bluetooth state below does.
    fn bt_generation(&self) -> u32;
    /// The paired keyboards and remotes, in first-seen order.
    fn bt_paired(&self) -> &[BtDevice];
    /// The devices found by discovery, in first-seen order.
    fn bt_found(&self) -> &[BtDevice];
    /// The Wireless screen's Bluetooth row value ("2 paired").
    fn bt_summary(&self) -> &str;
    /// The Bluetooth screen's header suffix ("Searching...").
    fn bt_status(&self) -> &str;
    /// Discovery is running.
    fn bt_scanning(&self) -> bool;
    /// Discovery may start (not during a wireless phone session).
    fn bt_scan_allowed(&self) -> bool;
    /// A phone is connected over Bluetooth (the home screen's icon).
    fn bt_phone_connected(&self) -> bool;
    /// The manager's dialog, if any.
    fn bt_prompt(&self) -> &Prompt;
    /// The dialog's accept button.
    fn bt_prompt_accept(&mut self);
    /// The dialog's Cancel (also Back, Home, and a key that cancels it).
    fn bt_prompt_cancel(&mut self);
    /// Start or stop discovery.
    fn bt_scan(&mut self, on: bool);
    /// Pair with a found device.
    fn bt_pair(&mut self, id: &str);
    /// Connect to or disconnect from a paired device.
    fn bt_connect(&mut self, id: &str, on: bool);
    /// Unpair a device.
    fn bt_forget(&mut self, id: &str);
    /// A Bluetooth screen is visible: discovery runs only then.
    fn bt_screen_visible(&mut self, visible: bool);

    // ── devices ──────────────────────────────────────────────────────────

    /// The phones the head unit knows, in the order the Devices screen
    /// shows them (see [`KnownDevice`]).
    fn devices(&self) -> &[KnownDevice];
    /// Changes whenever the list above does.
    fn devices_generation(&self) -> u32;
    /// Connect: wake this phone -- a session on it, over the link it is on;
    /// the phone in session now, if another, is stopped first.
    fn device_connect(&mut self, id: &str);
    /// Stop: end the session on this phone (the one in session) and hold
    /// it, so it does not take the head unit again by itself.
    fn device_stop(&mut self, id: &str);
    /// Forget: unpair this phone on every link and drop its record. The
    /// screens ask first.
    fn device_forget(&mut self, id: &str);

    // ── now playing ──────────────────────────────────────────────────────

    /// What the phone plays (see [`Media`]).
    fn media(&self) -> &Media;
    /// Changes whenever the above does.
    fn media_generation(&self) -> u32;
    /// A transport key from the Media page, for the phone.
    fn media_key(&mut self, key: MediaKey);
    /// The album art, once each time it changed: a picture, or none
    /// (`Art::default()`) when what plays has no art.
    fn take_art(&mut self) -> Option<Art>;

    // ── route guidance ───────────────────────────────────────────────────

    /// The navigation card (see [`NavCard`]).
    fn nav_card(&self) -> &NavCard;
    /// Changes with every guidance update: each one keeps the card up.
    fn nav_generation(&self) -> u32;

    // ── update ───────────────────────────────────────────────────────────

    /// Settings > Update (see [`Update`]).
    fn update(&self) -> &Update;
    /// Changes whenever the above does.
    fn update_generation(&self) -> u32;
    /// "Get from phone" opened: serve the page on the access point.
    fn update_serve(&mut self);
    /// The Update pages show, or were left (or the phone's video covers
    /// them): the page stops a while after.
    fn update_screens(&mut self, visible: bool);
    /// Install the checked update, once asked: the phone's session ends
    /// first, then the installer runs and restarts the head unit.
    fn update_install(&mut self);
    /// Cancel the update, once asked: a transfer stops, what arrived goes.
    fn update_cancel(&mut self);
    /// The last update's outcome was shown.
    fn update_outcome_seen(&mut self);

    // ── info ─────────────────────────────────────────────────────────────

    fn system_names(&self) -> SystemNames;
    /// The memory and CPU counters, read now.
    fn system_load(&mut self) -> lehu_core::sysinfo::Load;
    /// Bytes of the phone's video given to the decoder running, wrapping.
    fn video_bytes(&self) -> u32;
}
