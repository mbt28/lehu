// SPDX-License-Identifier: GPL-3.0-or-later
//! What the screens see of the program: the settings and their persistence,
//! the volume, the session, the camera, Bluetooth, power, the update.

use std::sync::Arc;

use lehu_camera::{Camera, Save};
use lehu_core::clock;
use lehu_core::devices::{Presence, Registry};
use lehu_core::settings::{self, Settings, UserFile};
use lehu_drm::Display;
use lehu_ui::{
    Art, AudioOutput, BtDevice, CameraLevel, CameraRequest, Host, KnownDevice, Media, MediaKey,
    NavCard, Power, Prompt, SystemNames, Update, UpdateOutcome, UpdateStage,
};
use lehu_update::Updater;
use lehu_wifi::{AccessPoint, Applied, Link, LinkProbe, Radio};
use serde::Serialize;
use toml::{Table, Value};

use crate::audio::Audio;

/// What the Devices screen asked of the loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceAction {
    Connect(String),
    Stop(String),
    Forget(String),
    /// The answer to "Switch to <phone>?".
    Switch(bool),
}

/// Bluetooth as the screens see it, copied from the manager's snapshot
/// when its generation moves: the lists, the dialog, the one-line status.
#[derive(Default)]
struct BtView {
    seen: Option<u32>,
    paired: Vec<BtDevice>,
    found: Vec<BtDevice>,
    summary: String,
    status: String,
    scanning: bool,
    scan_allowed: bool,
    phone_connected: bool,
    prompt: Prompt,
}

fn bt_device(d: &lehu_bt::Device) -> BtDevice {
    BtDevice {
        id: d.id.clone(),
        name: d.name.clone(),
        detail: d.detail.clone(),
        connected: d.connected,
    }
}

fn bt_prompt(p: &lehu_bt::Prompt) -> Prompt {
    Prompt {
        kind: match p.kind {
            lehu_bt::PromptKind::None => lehu_ui::PromptKind::None,
            lehu_bt::PromptKind::Code => lehu_ui::PromptKind::Code,
            lehu_bt::PromptKind::Question => lehu_ui::PromptKind::Question,
        },
        title: p.title.clone(),
        code: p.code.clone(),
        hint: p.hint.clone(),
        accept: p.accept.clone(),
    }
}

/// Before this (2024-01-01, Unix seconds) the system clock was never set:
/// a board without a battery-backed clock starts in 1970.
const CLOCK_SET_S: u64 = 1_704_067_200;

/// The time of day, milliseconds since midnight: in the phone's zone once
/// a phone has told it (the system clock is set in UTC, and the board
/// usually has no zone of its own), else in the system's zone. `None`
/// while the clock is not set. The bar's clock asks once a minute.
fn local_time_ms(zone_min: Option<i32>) -> Option<u64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    if now.as_secs() < CLOCK_SET_S {
        return None;
    }
    if let Some(zone) = zone_min {
        let local = now.as_millis() as i64 + i64::from(zone) * 60_000;
        return u64::try_from(local.rem_euclid(86_400_000)).ok();
    }
    let secs = now.as_secs() as libc::time_t;
    // SAFETY: a zeroed tm filled in by localtime_r, which is reentrant and
    // reads only `secs`.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return None;
        }
        tm
    };
    let day_s = u64::try_from(tm.tm_hour * 3600 + tm.tm_min * 60 + tm.tm_sec).ok()?;
    Some(day_s * 1000 + u64::from(now.subsec_millis()))
}

/// Changes are written this long after the last one: the knob turns in
/// bursts, and the flash is written once per burst.
const SAVE_AFTER_MS: u64 = 2000;

pub struct AppHost {
    settings: Settings,
    user: UserFile,
    image: Table,
    changed_at: Option<u64>,
    /// Counts the settings' changes: the loop applies them when it moves.
    settings_gen: u32,
    audio: Arc<Audio>,
    bt: Option<Arc<lehu_bt::Manager>>,
    bt_view: BtView,
    /// The access point was reconfigured: the wireless session starts over.
    ap_changed: bool,
    /// The known phones as the Devices screen lists them, copied from the
    /// registry when its generation moves.
    devices: Vec<KnownDevice>,
    devices_gen: u32,
    /// What the Devices screen asked, for the loop to carry out.
    device_action: Option<DeviceAction>,
    /// "Switch to <phone>?", shown as Bluetooth's dialog is when Bluetooth
    /// has none: its generation, and the dialog.
    question_gen: u32,
    question: Option<Prompt>,
    status: String,
    backgrounded: bool,
    usb: bool,
    wifi: bool,
    resume: bool,
    power: Option<Power>,
    camera_request: CameraRequest,
    /// The device, when there is a display to show it on.
    camera: Option<Camera>,
    camera_present: bool,
    camera_wanted: bool,
    camera_failure: Option<String>,
    cam_levels: Vec<CameraLevel>,
    cam_generation: u32,
    learning: Option<bool>,
    learned: Option<u32>,
    prompt: Prompt,
    none_levels: Vec<CameraLevel>,
    none_devices: Vec<BtDevice>,
    /// What the phone plays and the navigation card, copied from the
    /// driver when its generations move.
    media: Media,
    media_gen: u32,
    nav: NavCard,
    nav_gen: u32,
    /// The phone's zone, minutes east of UTC, once one has said.
    zone_min: Option<i32>,
    /// A transport key from the Media page, for the loop to send.
    media_key: Option<MediaKey>,
    /// The playback devices, as last listed for the Audio screen.
    outputs: Vec<AudioOutput>,
    /// The Link speed page's sampler, made when the page first asks.
    link: Option<LinkProbe>,
    /// The album art, decoded, until the screens take it.
    art: Option<Art>,
    /// Settings > Update, when an update server is set; the view the
    /// screens see, copied when the updater's generation moves, and our
    /// own generation for them.
    updater: Option<Updater>,
    update: Update,
    update_seen: Option<u32>,
    update_gen: u32,
    /// Install was confirmed: the loop ends the phone's session first.
    update_install: bool,
    video_bytes: u32,
}

impl AppHost {
    pub fn new(
        settings: Settings,
        user: UserFile,
        image: Table,
        display: Option<Arc<Display>>,
        audio: Arc<Audio>,
        bt: Option<Arc<lehu_bt::Manager>>,
        updater: Option<Updater>,
    ) -> Self {
        // A camera needs the video plane; the probe opens the device for a
        // moment, as FastCarPlay did, so the Video screen knows its rows.
        let camera_present = display.is_some() && lehu_camera::present(&settings.camera.device);
        let camera = display.map(Camera::new);
        Self {
            settings,
            user,
            image,
            changed_at: None,
            settings_gen: 0,
            audio,
            bt,
            bt_view: BtView::default(),
            ap_changed: false,
            devices: Vec::new(),
            devices_gen: 0,
            device_action: None,
            question_gen: 0,
            question: None,
            status: String::new(),
            backgrounded: false,
            usb: false,
            wifi: false,
            resume: false,
            power: None,
            camera_request: CameraRequest::None,
            camera,
            camera_present,
            camera_wanted: false,
            camera_failure: None,
            cam_levels: Vec::new(),
            cam_generation: 0,
            learning: None,
            learned: None,
            prompt: Prompt::default(),
            none_levels: Vec::new(),
            none_devices: Vec::new(),
            media: Media::default(),
            media_gen: 0,
            nav: NavCard::default(),
            nav_gen: 0,
            zone_min: None,
            media_key: None,
            outputs: Vec::new(),
            link: None,
            art: None,
            update: Update {
                shown: updater.is_some(),
                ..Update::default()
            },
            updater,
            update_seen: None,
            update_gen: 0,
            update_install: false,
            video_bytes: 0,
        }
    }

    /// The driver's now playing generation the page was copied at.
    pub fn media_seen(&self) -> u32 {
        self.media_gen
    }

    pub fn set_media(&mut self, (generation, np): (u32, &lehu_session::NowPlaying)) {
        self.media_gen = generation;
        crate::media::media_of(np, &mut self.media);
    }

    /// The phone's zone, for the bar's clock.
    pub fn set_zone(&mut self, zone_min: Option<i32>) {
        self.zone_min = zone_min;
    }

    /// The driver's guidance generation the card was composed at.
    pub fn nav_seen(&self) -> u32 {
        self.nav_gen
    }

    pub fn set_guidance(
        &mut self,
        (generation, g): (u32, &lehu_session::Guidance),
        zone_min: Option<i32>,
    ) {
        self.nav_gen = generation;
        crate::media::card_of(g, zone_min, &mut self.nav);
    }

    /// The album art the driver decoded: the screens take it next.
    pub fn set_art(&mut self, art: Art) {
        self.art = Some(art);
    }

    /// A transport key the Media page asked for since the last call.
    pub fn take_media_key(&mut self) -> Option<MediaKey> {
        self.media_key.take()
    }

    /// Every loop turn: the debounced save of the user's file, the camera's
    /// column kept current.
    pub fn tick(&mut self) {
        self.sync_camera_levels();
        self.sync_bluetooth();
        if let Some(level) = self.audio.volume.tick() {
            self.persist_any("audio.volume", level);
        }
        if let Some(at) = self.changed_at
            && clock::now_ms().saturating_sub(at) >= SAVE_AFTER_MS
        {
            self.changed_at = None;
            match self.user.save_if_dirty() {
                Ok(true) => log::info!("settings saved to {}", self.user.path().display()),
                Ok(false) => {}
                Err(e) => log::error!("{e}"),
            }
        }
    }

    /// Shutdown: the audio released, a pending save written now.
    pub fn flush(&mut self) {
        if let Some(level) = self.audio.stop() {
            self.persist_any("audio.volume", level);
        }
        if let Err(e) = self.user.save_if_dirty() {
            log::error!("{e}");
        }
    }

    /// Moves with every change of the settings.
    pub fn settings_generation(&self) -> u32 {
        self.settings_gen
    }

    pub fn take_power(&mut self) -> Option<Power> {
        self.power.take()
    }

    /// The manager's snapshot, copied when it changed; a clone of a few
    /// strings, a handful of times a minute at most.
    fn sync_bluetooth(&mut self) {
        let Some(bt) = &self.bt else { return };
        let generation = bt.generation();
        if self.bt_view.seen == Some(generation) {
            return;
        }
        let snap = bt.snapshot();
        let v = &mut self.bt_view;
        v.seen = Some(generation);
        v.paired = snap.paired.iter().map(bt_device).collect();
        v.found = snap.found.iter().map(bt_device).collect();
        v.summary = snap.summary;
        v.status = snap.status;
        v.scanning = snap.scanning;
        v.scan_allowed = snap.scan_allowed;
        v.phone_connected = snap.phone_connected;
        v.prompt = bt_prompt(&snap.prompt);
    }

    /// The updater's view, copied when it changed: a few strings, a few
    /// times a second at most while an update arrives. Before the screens
    /// tick: the updater's wake is the only one there may be.
    pub fn sync_update(&mut self) {
        let Some(updater) = &self.updater else { return };
        let generation = updater.generation();
        if self.update_seen == Some(generation) {
            return;
        }
        self.update_seen = Some(generation);
        self.update_gen = self.update_gen.wrapping_add(1);
        let v = updater.view();
        let u = &mut self.update;
        u.can_cancel = v.can_cancel();
        u.available = v.available;
        u.version = v.booted_version;
        u.stage = match v.stage {
            lehu_update::Stage::Idle => UpdateStage::Idle,
            lehu_update::Stage::Receiving => UpdateStage::Receiving,
            lehu_update::Stage::Checking => UpdateStage::Checking,
            lehu_update::Stage::Ready => UpdateStage::Ready,
            lehu_update::Stage::Failed => UpdateStage::Failed,
            lehu_update::Stage::Installing => UpdateStage::Installing,
            lehu_update::Stage::Restarting => UpdateStage::Restarting,
        };
        u.received = v.received;
        u.size = v.size;
        u.new_version = v.version;
        u.notes = v.notes;
        u.size_kb = v.size_kb;
        u.restart = v.mode == lehu_update::Mode::UpdateBoot;
        u.percent = v.percent;
        u.message = v.message;
        u.serving = v.serving;
        u.url = v.url;
        u.serve_error = v.serve_error;
        u.outcome = match v.outcome {
            None => UpdateOutcome::None,
            Some(lehu_update::Outcome::Updated(version)) => UpdateOutcome::Updated(version),
            Some(lehu_update::Outcome::Failed(why)) => UpdateOutcome::Failed(why),
            Some(lehu_update::Outcome::Interrupted) => UpdateOutcome::Interrupted,
        };
    }

    /// Install was confirmed since the last call: the loop ends the
    /// phone's session, then calls `start_install`.
    pub fn take_update_install(&mut self) -> bool {
        std::mem::take(&mut self.update_install)
    }

    /// The installer starts; false when there was nothing to install.
    pub fn start_install(&mut self) -> bool {
        let started = self.updater.as_ref().is_some_and(Updater::install);
        self.sync_update();
        started
    }

    /// An install runs, or the head unit is about to restart.
    pub fn update_busy(&self) -> bool {
        self.updater.as_ref().is_some_and(Updater::busy)
    }

    /// The registry's generation the list was copied at.
    pub fn devices_seen(&self) -> u32 {
        self.devices_gen
    }

    /// Copy the registry into the screen's rows: the session's phone first,
    /// then the ones on a link, then the rest by last seen. The ordering is
    /// LIVI's (livi-core/src/devices.rs, Lasse Heitgres, GPL-3.0).
    pub fn set_devices(&mut self, registry: &Registry) {
        self.devices_gen = registry.generation();
        let mut all: Vec<_> = registry.all().iter().collect();
        all.sort_by(|a, b| {
            let rank = |p: Presence| match p {
                Presence::Active => 0,
                Presence::Available => 1,
                Presence::Offline => 2,
            };
            rank(a.presence)
                .cmp(&rank(b.presence))
                .then(b.last_seen.cmp(&a.last_seen))
        });
        self.devices.clear();
        for d in all {
            let (place, speaks) = (
                match d.presence {
                    Presence::Active => "Connected",
                    _ if d.held => "Stopped",
                    Presence::Available => "Nearby",
                    Presence::Offline => "Not here",
                },
                match d.kind {
                    lehu_core::devices::Kind::CarPlay => "CarPlay",
                    lehu_core::devices::Kind::AndroidAuto => "Android Auto",
                },
            );
            self.devices.push(KnownDevice {
                id: d.id.clone(),
                name: d.name.clone(),
                detail: format!("{place} - {speaks}"),
                active: d.presence == Presence::Active,
                nearby: d.presence == Presence::Available,
            });
        }
    }

    /// What the Devices screen asked since the last call.
    pub fn take_device_action(&mut self) -> Option<DeviceAction> {
        self.device_action.take()
    }

    /// The driver's switch question, mirrored each turn: (its generation,
    /// the phone that came, the phone in session).
    pub fn set_switch_question(&mut self, (generation, question): (u32, Option<(&str, &str)>)) {
        if generation == self.question_gen {
            return;
        }
        self.question_gen = generation;
        self.question = question.map(|(phone, current)| Prompt {
            kind: lehu_ui::PromptKind::Question,
            title: format!("Switch to {phone}?"),
            code: String::new(),
            hint: format!("{current} stops, and {phone} takes the screen."),
            accept: "Switch".into(),
        });
    }

    /// Bluetooth has a dialog of its own up: it goes first.
    fn bt_prompt_up(&self) -> bool {
        self.bt.is_some() && self.bt_view.prompt.kind != lehu_ui::PromptKind::None
    }

    /// The access point was reconfigured since the last call.
    pub fn take_ap_changed(&mut self) -> bool {
        std::mem::take(&mut self.ap_changed)
    }

    /// The screens asked to go back to the phone (Menu on the home screen,
    /// the Resume row).
    pub fn take_resume(&mut self) -> bool {
        std::mem::take(&mut self.resume)
    }

    /// What the session driver knows, for the status line and the rows
    /// that depend on a phone being there.
    pub fn set_session(&mut self, status: Option<&str>, backgrounded: bool, usb: bool, wifi: bool) {
        if let Some(s) = status {
            self.status.clear();
            self.status.push_str(s);
        }
        self.backgrounded = backgrounded;
        self.usb = usb;
        self.wifi = wifi;
    }

    /// Counted by the video decoder, for the Info page.
    pub fn set_video_bytes(&mut self, bytes: u32) {
        self.video_bytes = bytes;
    }

    /// The input thread's learning switch, asked for by the screens.
    pub fn take_learning(&mut self) -> Option<bool> {
        self.learning.take()
    }

    pub fn key_learned(&mut self, code: u16) {
        self.learned = Some(u32::from(code));
    }

    pub fn request_camera(&mut self, request: CameraRequest) {
        self.camera_request = request;
    }

    pub fn toggle_mute(&mut self) {
        self.audio.volume.toggle_mute();
    }

    pub fn camera_key(&self) -> u32 {
        self.settings.camera.key
    }

    /// The camera view is up: the loop should run the camera.
    pub fn camera_wanted(&self) -> bool {
        self.camera_wanted
    }

    pub fn camera_mut(&mut self) -> Option<&mut Camera> {
        self.camera.as_mut()
    }

    /// The camera could not start or stopped: the view shows why.
    pub fn camera_failed(&mut self, why: &str) {
        self.camera_failure = Some(why.to_string());
    }

    /// What the camera asks to be saved: the input shown and the profiles,
    /// each only when it differs from the settings.
    pub fn persist_camera(&mut self, save: Save) {
        if save.input != self.settings.camera.input {
            self.persist_any("camera.input", save.input);
        }
        if save.inputs != self.settings.camera.inputs {
            self.persist_any("camera.inputs", &save.inputs);
        }
    }

    /// The column's levels, refreshed when the camera says they changed.
    fn sync_camera_levels(&mut self) {
        let Some(camera) = &self.camera else { return };
        if camera.generation() == self.cam_generation && !self.cam_levels.is_empty() {
            return;
        }
        self.cam_generation = camera.generation();
        self.cam_levels.clear();
        self.cam_levels
            .extend(camera.levels().iter().map(|l| CameraLevel {
                label: l.label.to_string(),
                value: l.value.clone(),
            }));
    }

    fn persist_any<T: Serialize>(&mut self, key: &str, value: T) {
        let done = self
            .user
            .set(key, value)
            .and_then(|()| settings::rebuild(&self.image, self.user.table()));
        match done {
            Ok(rebuilt) => {
                self.settings = rebuilt;
                self.changed_at = Some(clock::now_ms());
                self.settings_gen = self.settings_gen.wrapping_add(1);
            }
            Err(e) => log::error!("settings: {key}: {e}"),
        }
    }
}

impl Host for AppHost {
    fn settings(&self) -> &Settings {
        &self.settings
    }

    fn persist(&mut self, key: &str, value: Value) -> Result<(), settings::Error> {
        self.user.set(key, value)?;
        self.settings = settings::rebuild(&self.image, self.user.table())?;
        // The streams' levels follow the settings at once.
        self.audio.set_levels(&self.settings.audio);
        self.changed_at = Some(clock::now_ms());
        self.settings_gen = self.settings_gen.wrapping_add(1);
        Ok(())
    }

    fn apply_audio_buffer(&mut self, ms: u32) {
        self.settings.audio.buffer_ms = ms;
        self.audio.set_buffer_ms(ms);
    }

    fn configure_access_point(
        &mut self,
        ssid: &str,
        passphrase: &str,
    ) -> Result<Applied, lehu_wifi::Error> {
        let applied = AccessPoint::configure(&self.settings.wifi, ssid, passphrase)?;
        // What the phone is told comes from the same file hostapd runs
        // from; the settings mirror it for the screens.
        let now = AccessPoint::read(&self.settings.wifi);
        self.persist_any("wifi.ssid", now.ssid.as_str());
        self.persist_any("wifi.passphrase", now.passphrase.as_str());
        self.ap_changed = true;
        Ok(applied)
    }

    fn access_point(&mut self) -> Result<Radio, lehu_wifi::Error> {
        AccessPoint::read_radio(&self.settings.wifi)
    }

    fn configure_radio(&mut self, radio: &Radio) -> Result<Applied, lehu_wifi::Error> {
        let applied = AccessPoint::configure_radio(&self.settings.wifi, radio)?;
        // The channel the phones are told is read from the file again when
        // the wireless listeners start over.
        if radio.channel != self.settings.wifi.channel {
            self.persist_any("wifi.channel", radio.channel);
        }
        self.ap_changed = true;
        Ok(applied)
    }

    fn audio_outputs(&mut self) -> &[AudioOutput] {
        let device = &self.settings.audio.device;
        self.outputs.clear();
        self.outputs
            .extend(lehu_audio::outputs().into_iter().map(|o| AudioOutput {
                active: lehu_audio::opens(device, &o),
                device: o.device,
                label: o.label,
            }));
        &self.outputs
    }

    fn link(&mut self) -> Link {
        let iface = &self.settings.wifi.interface;
        self.link
            .get_or_insert_with(|| LinkProbe::new(iface))
            .sample()
    }

    fn volume_level(&self) -> u32 {
        self.audio.volume.level()
    }

    fn volume_muted(&self) -> bool {
        self.audio.volume.muted()
    }

    fn volume_generation(&self) -> u32 {
        self.audio.volume.generation()
    }

    fn volume_step(&mut self, direction: i32) {
        self.audio.volume.step(direction);
    }

    fn status(&self) -> &str {
        &self.status
    }

    fn local_time_ms(&self) -> Option<u64> {
        local_time_ms(self.zone_min)
    }

    fn session_backgrounded(&self) -> bool {
        self.backgrounded
    }

    fn link_usb(&self) -> bool {
        self.usb
    }

    fn link_wifi(&self) -> bool {
        self.wifi
    }

    fn request_resume(&mut self) {
        self.resume = true;
    }

    fn night_mode_changed(&mut self) {}

    fn request_power(&mut self, action: Power) {
        self.power = Some(action);
    }

    fn camera_present(&self) -> bool {
        self.camera_present
    }

    fn camera_inputs(&self) -> usize {
        self.camera.as_ref().map_or(0, |c| c.inputs().len())
    }

    fn camera_input(&self) -> usize {
        self.camera.as_ref().map_or(0, Camera::input)
    }

    fn camera_no_signal(&self) -> bool {
        self.camera.as_ref().is_none_or(Camera::no_signal)
    }

    fn camera_has_standard(&self) -> bool {
        self.camera.as_ref().is_some_and(Camera::has_standard)
    }

    fn camera_standard_label(&self) -> &str {
        self.camera.as_ref().map_or("", Camera::standard_label)
    }

    fn camera_levels(&self) -> &[CameraLevel] {
        if self.camera.is_some() {
            &self.cam_levels
        } else {
            &self.none_levels
        }
    }

    fn camera_generation(&self) -> u32 {
        self.camera.as_ref().map_or(0, Camera::generation)
    }

    fn camera_select_input(&mut self, index: usize) {
        if let Some(c) = &mut self.camera {
            c.select_input(index);
        }
        self.sync_camera_levels();
    }

    fn camera_cycle_standard(&mut self, direction: i32) {
        if let Some(c) = &mut self.camera {
            c.cycle_standard(direction);
        }
        self.sync_camera_levels();
    }

    fn camera_adjust_level(&mut self, index: usize, direction: i32) {
        if let Some(c) = &mut self.camera {
            c.adjust_level(index, direction);
        }
        self.sync_camera_levels();
    }

    fn set_camera_running(&mut self, on: bool) {
        self.camera_wanted = on;
    }

    fn take_camera_failure(&mut self, why: &mut String) -> bool {
        match self.camera_failure.take() {
            Some(text) => {
                *why = text;
                true
            }
            None => false,
        }
    }

    fn take_camera_request(&mut self) -> CameraRequest {
        std::mem::take(&mut self.camera_request)
    }

    fn set_key_learning(&mut self, on: bool) {
        self.learning = Some(on);
    }

    fn take_learned_key(&mut self) -> Option<u32> {
        self.learned.take()
    }

    fn bt_available(&self) -> bool {
        self.bt.is_some()
    }

    fn bt_generation(&self) -> u32 {
        // The switch question rides on Bluetooth's dialog: a change of
        // either is a change for the screens.
        self.bt_view
            .seen
            .unwrap_or(0)
            .wrapping_add(self.question_gen.wrapping_mul(0x9e37_79b9))
    }

    fn bt_paired(&self) -> &[BtDevice] {
        if self.bt.is_some() {
            &self.bt_view.paired
        } else {
            &self.none_devices
        }
    }

    fn bt_found(&self) -> &[BtDevice] {
        if self.bt.is_some() {
            &self.bt_view.found
        } else {
            &self.none_devices
        }
    }

    fn bt_summary(&self) -> &str {
        &self.bt_view.summary
    }

    fn bt_status(&self) -> &str {
        &self.bt_view.status
    }

    fn bt_scanning(&self) -> bool {
        self.bt_view.scanning
    }

    fn bt_scan_allowed(&self) -> bool {
        self.bt_view.scan_allowed
    }

    fn bt_phone_connected(&self) -> bool {
        self.bt_view.phone_connected
    }

    fn bt_prompt(&self) -> &Prompt {
        if self.bt_prompt_up() {
            &self.bt_view.prompt
        } else if let Some(q) = &self.question {
            q
        } else {
            &self.prompt
        }
    }

    fn bt_prompt_accept(&mut self) {
        if self.bt_prompt_up() {
            if let Some(bt) = &self.bt {
                bt.prompt_accept();
            }
        } else if self.question.is_some() {
            self.device_action = Some(DeviceAction::Switch(true));
        }
    }

    fn bt_prompt_cancel(&mut self) {
        if self.bt_prompt_up() {
            if let Some(bt) = &self.bt {
                bt.prompt_cancel();
            }
        } else if self.question.is_some() {
            self.device_action = Some(DeviceAction::Switch(false));
        }
    }

    fn bt_scan(&mut self, on: bool) {
        if let Some(bt) = &self.bt {
            bt.scan(on);
        }
    }

    fn bt_pair(&mut self, id: &str) {
        if let Some(bt) = &self.bt {
            bt.pair(id);
        }
    }

    fn bt_connect(&mut self, id: &str, on: bool) {
        if let Some(bt) = &self.bt {
            bt.connect(id, on);
        }
    }

    fn bt_forget(&mut self, id: &str) {
        if let Some(bt) = &self.bt {
            bt.forget(id);
        }
    }

    fn bt_screen_visible(&mut self, visible: bool) {
        if let Some(bt) = &self.bt {
            bt.screen_visible(visible);
        }
    }

    fn devices(&self) -> &[KnownDevice] {
        &self.devices
    }

    fn devices_generation(&self) -> u32 {
        self.devices_gen
    }

    fn device_connect(&mut self, id: &str) {
        self.device_action = Some(DeviceAction::Connect(id.to_string()));
    }

    fn device_stop(&mut self, id: &str) {
        self.device_action = Some(DeviceAction::Stop(id.to_string()));
    }

    fn device_forget(&mut self, id: &str) {
        self.device_action = Some(DeviceAction::Forget(id.to_string()));
    }

    fn media(&self) -> &Media {
        &self.media
    }

    fn media_generation(&self) -> u32 {
        self.media_gen
    }

    fn media_key(&mut self, key: MediaKey) {
        self.media_key = Some(key);
    }

    fn take_art(&mut self) -> Option<Art> {
        self.art.take()
    }

    fn nav_card(&self) -> &NavCard {
        &self.nav
    }

    fn nav_generation(&self) -> u32 {
        self.nav_gen
    }

    fn update(&self) -> &Update {
        &self.update
    }

    fn update_generation(&self) -> u32 {
        self.update_gen
    }

    fn update_serve(&mut self) {
        let Some(updater) = &self.updater else { return };
        // The access point as hostapd runs it: what the phone joins, and
        // the address the page is served on.
        let ap = AccessPoint::read(&self.settings.wifi);
        self.update.ssid = ap.ssid;
        self.update.passphrase = ap.passphrase;
        updater.serve(ap.ip.parse().ok());
        self.sync_update();
    }

    fn update_screens(&mut self, visible: bool) {
        if let Some(updater) = &self.updater {
            updater.screens(visible);
        }
    }

    fn update_install(&mut self) {
        self.update_install = true;
    }

    fn update_cancel(&mut self) {
        if let Some(updater) = &self.updater {
            updater.cancel();
        }
        self.sync_update();
    }

    fn system_names(&self) -> SystemNames {
        SystemNames {
            kernel: lehu_core::sysinfo::kernel(),
            os: lehu_core::sysinfo::os_name(),
            lehu: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    fn system_load(&mut self) -> lehu_core::sysinfo::Load {
        lehu_core::sysinfo::load()
    }

    fn video_bytes(&self) -> u32 {
        self.video_bytes
    }

    fn update_outcome_seen(&mut self) {
        if let Some(updater) = &self.updater {
            updater.outcome_seen();
        }
        self.sync_update();
    }
}
