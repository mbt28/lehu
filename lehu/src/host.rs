// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
// Copyright (C) 2025-2026 Lasse Heitgres (LIVI, https://github.com/f-io/LIVI)
//! What the screens see of the program: the settings and their persistence,
//! the volume, the session, the camera, Bluetooth, power, the update.

use std::sync::Arc;

use lehu_camera::{Camera, Save};
use lehu_core::clock;
use lehu_core::devices::{Presence, Registry};
use lehu_core::settings::{self, Settings, UserFile};
use lehu_drm::Display;
use lehu_map::{Ahead, Drive, Marker, Route, TileSet, Track, Turn, View, Viewer};
use lehu_ui::{
    Art, AudioOutput, BtDevice, CameraLevel, CameraRequest, Host, KnownDevice, Media, MediaKey,
    NavArrow, NavCard, Power, Prompt, SystemNames, Update, UpdateOutcome, UpdateStage,
};
use lehu_update::Updater;
use lehu_wifi::{AccessPoint, Applied, Link, LinkProbe, Radio};
use serde::Serialize;
use toml::{Table, Value};

use crate::audio::Audio;
use crate::voice::Speaker;

/// The map's bar follows a replayed drive this often (the map itself
/// moves along it by itself, in step with the panel).
const MAP_TRACK_STEP_MS: u64 = 200;

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
    /// The map: the display it goes on, the viewer while its screen is up,
    /// the tile set and the view it shows.
    map_display: Option<Arc<Display>>,
    map: Option<Viewer>,
    map_set: Option<TileSet>,
    map_view: View,
    map_wanted: bool,
    /// Where we are, world pixels at zoom 22, and what the map shows there.
    map_position: Option<(i64, i64)>,
    map_marker: Option<Marker>,
    /// A drive replayed as the position (map.fake-track), and when it
    /// started: it goes on while the map is closed.
    map_track: Option<Arc<Track>>,
    map_track_start: Option<u64>,
    map_track_shown: u64,
    /// The map keeps where we are in the middle, until it is dragged.
    map_follow: bool,
    /// The replayed drive as a route, drawn on the map, and the guidance
    /// along it for the map's bar.
    map_route: Option<Route>,
    map_guide: Option<NavCard>,
    /// The drive's speed, km/h, and the road's limit, for the bar.
    map_speed: Option<(u32, Option<u16>)>,
    /// The spoken turn prompts along the drive (map.voice): what has been
    /// said, and the clips. Kept while the map is closed, as the drive goes
    /// on; its clips are not.
    map_voice: Option<Speaker>,
    map_note: String,
    map_label: String,
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
        let map_display = display.clone();
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
            map_display,
            map: None,
            map_set: None,
            map_view: View {
                zoom: 0,
                x: 0,
                y: 0,
            },
            map_wanted: false,
            map_position: None,
            map_marker: None,
            map_track: None,
            map_track_start: None,
            map_track_shown: 0,
            map_follow: true,
            map_route: None,
            map_guide: None,
            map_speed: None,
            map_voice: None,
            map_note: String::new(),
            map_label: String::new(),
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

    /// The map's screen is up: the loop should show the map.
    pub fn map_wanted(&self) -> bool {
        self.map_wanted
    }

    /// The map is on the video plane, or on its way there.
    pub fn map_started(&self) -> bool {
        self.map.is_some()
    }

    /// Its first picture is up.
    pub fn map_ready(&self) -> bool {
        self.map.as_ref().is_some_and(Viewer::picture_ready)
    }

    /// Show the map: the tile set read afresh (the card may have new
    /// tiles), the view where it was left. False, with the reason on the
    /// screen, when there is no map or no memory for it.
    pub fn map_start(&mut self) -> bool {
        let Some(display) = self.map_display.clone() else {
            self.map_note = "No display for the map".into();
            return false;
        };
        let set = match TileSet::open(std::path::Path::new(&self.settings.map.dir)) {
            Ok(set) => set,
            Err(e) => {
                log::warn!("map: {e}");
                self.map_note = e;
                return false;
            }
        };
        let m = &self.settings.map;
        self.map_position = match lehu_map::parse_position(&m.fake_position) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("map.fake-position {e}");
                None
            }
        };
        self.map_marker = self.map_position.map(|(x, y)| Marker::Dot(x, y));
        self.map_track = None;
        self.map_route = None;
        self.map_guide = None;
        if !m.fake_track.is_empty() {
            match std::fs::read_to_string(&m.fake_track)
                .map_err(|e| e.to_string())
                .and_then(|text| Track::parse_gpx(&text))
            {
                Ok(track) => {
                    log::info!(
                        "map: replaying {} ({} s a lap)",
                        m.fake_track,
                        track.duration() / 1000
                    );
                    let start = *self
                        .map_track_start
                        .get_or_insert_with(lehu_core::clock::now_ms);
                    let fix = track.at(lehu_core::clock::now_ms() - start);
                    self.map_position = Some((fix.x, fix.y));
                    self.map_marker = Some(Marker::Arrow(fix.x, fix.y, fix.heading));
                    self.map_route = Some(Route::from(track.line()));
                    self.map_guide = track
                        .ahead(lehu_core::clock::now_ms() - start)
                        .map(|a| guidance(&a, local_time_ms(self.zone_min)));
                    self.map_speed = Some((fix.kmh, fix.limit));
                    self.map_track = Some(Arc::new(track));
                }
                Err(e) => log::warn!("map.fake-track {}: {e}", m.fake_track),
            }
        }
        self.map_follow = true;
        let mut view = if m.zoom == 0 {
            View {
                zoom: set.start_zoom,
                x: set.center.0,
                y: set.center.1,
            }
        } else {
            View {
                zoom: m.zoom,
                x: m.x,
                y: m.y,
            }
        };
        // Where we are, when we know: like any navigation map.
        if let Some((x, y)) = self.map_position {
            (view.x, view.y) = (x, y);
        }
        view.zoom_by(0, set.min_zoom, set.top_zoom());
        view.clamp(&set.bounds);
        // The night tiles, when the set has them.
        let night = match set.night() {
            Some(Ok(n)) => Some(n),
            Some(Err(e)) => {
                log::warn!("map: night tiles: {e}");
                None
            }
            None => None,
        };
        match Viewer::start(display, set.clone(), night, view) {
            Ok(viewer) => {
                viewer.set_night(self.settings.display.night_mode == settings::NightMode::Night);
                viewer.set_position(self.map_marker);
                viewer.set_route(self.map_route.clone());
                // A replayed drive: the map moves along it by itself.
                if let (Some(track), Some(start)) = (&self.map_track, self.map_track_start) {
                    let ago = lehu_core::clock::now_ms().saturating_sub(start);
                    let now = std::time::Instant::now();
                    viewer.set_drive(Some(Drive {
                        track: Arc::clone(track),
                        start: now
                            .checked_sub(std::time::Duration::from_millis(ago))
                            .unwrap_or(now),
                        bounds: set.bounds,
                        top_zoom: set.top_zoom(),
                    }));
                    viewer.follow(self.map_follow.then_some(view.zoom));
                }
                self.map = Some(viewer);
                self.map_view = view;
                self.map_set = Some(set);
                self.map_note = "Loading the map".into();
                self.update_map_label();
                true
            }
            Err(e) => {
                log::warn!("map: {e}");
                self.map_note = e;
                false
            }
        }
    }

    /// While a drive is replayed: how long the loop may sleep before the
    /// position should move on.
    pub fn map_due(&self) -> Option<std::time::Duration> {
        (self.map.is_some() && self.map_track.is_some()).then(|| {
            let next = self.map_track_shown + MAP_TRACK_STEP_MS;
            std::time::Duration::from_millis(next.saturating_sub(lehu_core::clock::now_ms()))
        })
    }

    /// Each turn of the loop while the map is up: where a replayed drive
    /// is, for the Me key and the bar (the map moves along it by itself).
    pub fn map_tick(&mut self) {
        let Some(start) = self.map_track_start else {
            return;
        };
        let now = lehu_core::clock::now_ms();
        if self.map.is_none() || now < self.map_track_shown + MAP_TRACK_STEP_MS {
            return;
        }
        let Some(track) = &self.map_track else {
            return;
        };
        let fix = track.at(now - start);
        let ahead = track.ahead(now - start);
        self.map_guide = ahead
            .as_ref()
            .map(|a| guidance(a, local_time_ms(self.zone_min)));
        // The prompts, while the map guides.
        let m = &self.settings.map;
        if m.voice {
            if self
                .map_voice
                .as_ref()
                .is_none_or(|v| v.dir() != m.voice_dir)
            {
                self.map_voice = Some(Speaker::new(&m.voice_dir));
            }
            if let (Some(speaker), Some(a)) = (&mut self.map_voice, &ahead) {
                let then = track.then(a).map(|(next, gap)| (next.turn, gap));
                speaker.tick(a, fix.kmh, then, &self.audio);
            }
        } else {
            self.map_voice = None;
        }
        self.map_track_shown = now;
        self.map_position = Some((fix.x, fix.y));
        self.map_speed = Some((fix.kmh, fix.limit));
    }

    /// Following a drive, the view the map shows (it moves by itself);
    /// otherwise the one it was given.
    fn map_view_now(&self) -> View {
        match &self.map {
            Some(viewer) if self.map_follow && self.map_track.is_some() => {
                // Its zoom is the one asked for, not one closer near a turn.
                View {
                    zoom: self.map_view.zoom,
                    ..viewer.view()
                }
            }
            _ => self.map_view,
        }
    }

    /// The map goes; where it was left is kept for next time.
    pub fn map_stop(&mut self) {
        self.map_view = self.map_view_now();
        if let Some(viewer) = self.map.take() {
            viewer.stop();
        }
        if let Some(speaker) = &mut self.map_voice {
            speaker.release();
        }
        let v = self.map_view;
        let m = &self.settings.map;
        if self.map_set.is_some() && (v.zoom, v.x, v.y) != (m.zoom, m.x, m.y) {
            self.persist_any("map.zoom", v.zoom);
            self.persist_any("map.x", v.x);
            self.persist_any("map.y", v.y);
        }
    }

    fn update_map_label(&mut self) {
        self.map_label.clear();
        if let Some(set) = &self.map_set {
            use std::fmt::Write as _;
            let _ = write!(
                self.map_label,
                "{}  -  zoom {}",
                set.name, self.map_view.zoom
            );
        }
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

    fn night_mode_changed(&mut self) {
        if let Some(viewer) = &self.map {
            viewer.set_night(self.settings.display.night_mode == settings::NightMode::Night);
        }
    }

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

    fn set_map_running(&mut self, on: bool) {
        self.map_wanted = on;
    }

    fn map_pan(&mut self, dx: i32, dy: i32) {
        // Moved by hand: from where it was, it stays where it was put,
        // north up.
        self.map_view = self.map_view_now();
        let (Some(set), Some(viewer)) = (&self.map_set, &self.map) else {
            return;
        };
        if self.map_follow {
            // As shown: a level closer near a turn.
            self.map_view.zoom = viewer.view().zoom;
        }
        viewer.follow(None);
        self.map_view.pan(dx, dy, &set.bounds);
        viewer.set_view(self.map_view);
        self.map_follow = false;
        viewer.set_heading(None);
    }

    fn map_zoom(&mut self, step: i32) {
        let (Some(set), Some(viewer)) = (&self.map_set, &self.map) else {
            return;
        };
        self.map_view.zoom_by(step, set.min_zoom, set.top_zoom());
        if self.map_follow && self.map_track.is_some() {
            viewer.follow(Some(self.map_view.zoom));
        } else {
            viewer.set_view(self.map_view);
        }
        self.update_map_label();
    }

    fn map_locate(&mut self) {
        let (Some(set), Some(viewer), Some((x, y))) = (&self.map_set, &self.map, self.map_position)
        else {
            return;
        };
        (self.map_view.x, self.map_view.y) = (x, y);
        self.map_view.clamp(&set.bounds);
        viewer.set_view(self.map_view);
        self.map_follow = true;
        if self.map_track.is_some() {
            viewer.follow(Some(self.map_view.zoom));
        }
    }

    fn map_guidance(&self) -> Option<&NavCard> {
        self.map.as_ref().and(self.map_guide.as_ref())
    }

    fn map_speed(&self) -> Option<(u32, Option<u16>)> {
        self.map.as_ref().and(self.map_speed)
    }

    fn map_has_position(&self) -> bool {
        self.map_position.is_some()
    }

    fn map_note(&self) -> &str {
        if self.map_ready() { "" } else { &self.map_note }
    }

    fn map_label(&self) -> &str {
        &self.map_label
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

/// The map's bar while it guides: the next turn, how far, onto what, and
/// what is left of the route.
/// The bar's guidance for the next turn. `time_of_day`: now, in ms since
/// local midnight, for the arrival as a clock time; None while the clock is
/// not set (then the minutes left).
fn guidance(a: &Ahead<'_>, time_of_day: Option<u64>) -> NavCard {
    let (arrow, maneuver) = match a.maneuver.turn {
        Turn::Straight => (NavArrow::Straight, "Continue".to_string()),
        Turn::SlightLeft => (NavArrow::SlightLeft, "Bear left".to_string()),
        Turn::Left => (NavArrow::Left, "Turn left".to_string()),
        Turn::SharpLeft => (NavArrow::SharpLeft, "Turn sharp left".to_string()),
        Turn::SlightRight => (NavArrow::SlightRight, "Bear right".to_string()),
        Turn::Right => (NavArrow::Right, "Turn right".to_string()),
        Turn::SharpRight => (NavArrow::SharpRight, "Turn sharp right".to_string()),
        Turn::UTurn => (NavArrow::UTurn, "Make a U-turn".to_string()),
        Turn::Roundabout(n) => (
            NavArrow::Roundabout,
            format!("Take the {} exit", ordinal(n)),
        ),
        Turn::Arrive => (NavArrow::Arrive, "Arrive".to_string()),
    };
    let road = match a.maneuver.turn {
        Turn::Arrive => "at the destination".to_string(),
        _ => a.maneuver.road.clone(),
    };
    let minutes = a.left_ms.div_ceil(60_000).max(1);
    let (eta, eta_note) = match time_of_day {
        Some(now) => {
            let at = (now + a.left_ms) / 60_000 % (24 * 60);
            (
                format!("{:02}:{:02}", at / 60, at % 60),
                format!("{minutes} min, {}", distance(a.left_metres)),
            )
        }
        None => (format!("{minutes} min"), distance(a.left_metres)),
    };
    NavCard {
        active: true,
        arrow,
        maneuver,
        distance: distance(a.metres),
        road,
        eta,
        eta_note,
    }
}

/// A distance as a driver reads it: the nearest 10 m under 100 m, 50 m
/// under a kilometre, a tenth of a kilometre under ten, then whole ones.
fn distance(metres: u32) -> String {
    match metres {
        0..10 => "Now".to_string(),
        10..95 => format!("{} m", (metres + 5) / 10 * 10),
        95..975 => format!("{} m", (metres + 25) / 50 * 50),
        975..9950 => {
            let tenths = (metres + 50) / 100;
            format!("{}.{} km", tenths / 10, tenths % 10)
        }
        _ => format!("{} km", (metres + 500) / 1000),
    }
}

/// 1st, 2nd, 3rd, 4th...
fn ordinal(n: u8) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

#[cfg(test)]
mod guidance_tests {
    use super::{distance, guidance, ordinal};

    #[test]
    fn the_arrival_is_a_clock_time_once_the_clock_is_set() {
        let gpx = r#"<gpx>
<wpt lat="49.6010" lon="6.1300"><name></name><type>arrive</type></wpt>
<trk><trkseg>
<trkpt lat="49.6000" lon="6.1300"><time>2027-01-15T08:00:00Z</time></trkpt>
<trkpt lat="49.6010" lon="6.1300"><time>2027-01-15T08:10:00Z</time></trkpt>
</trkseg></trk></gpx>"#;
        let track = lehu_map::Track::parse_gpx(gpx).unwrap();
        let ahead = track.ahead(0).unwrap();
        // 23:55 and ten minutes to go: past midnight.
        let card = guidance(&ahead, Some((23 * 60 + 55) * 60_000));
        assert_eq!(card.eta, "00:05");
        assert_eq!(card.eta_note, "10 min, 100 m");
        let card = guidance(&ahead, None);
        assert_eq!(card.eta, "10 min");
        assert_eq!(card.eta_note, "100 m");
    }

    #[test]
    fn distances_read_as_a_driver_reads_them() {
        assert_eq!(distance(4), "Now");
        assert_eq!(distance(44), "40 m");
        assert_eq!(distance(46), "50 m");
        assert_eq!(distance(330), "350 m");
        assert_eq!(distance(94), "90 m");
        assert_eq!(distance(960), "950 m");
        assert_eq!(distance(990), "1.0 km");
        assert_eq!(distance(9960), "10 km");
        assert_eq!(distance(1240), "1.2 km");
        assert_eq!(distance(1960), "2.0 km");
        assert_eq!(distance(12_600), "13 km");
        assert_eq!(ordinal(1), "1st");
        assert_eq!(ordinal(2), "2nd");
        assert_eq!(ordinal(12), "12th");
        assert_eq!(ordinal(23), "23rd");
    }
}
