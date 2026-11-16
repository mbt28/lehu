// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A `Host` for the tests: plain state the test sets, and a record of every
//! call the screens make. Settings are the real ones: `persist` writes the
//! user layer and rebuilds the settings in force from it, as the program
//! does.

use lehu_core::settings::{self, Paths, Settings, UserFile};
use lehu_wifi::{Applied, Band, Link, Radio};
use toml::Value;

use crate::host::{
    Art, AudioOutput, BtDevice, CameraLevel, CameraRequest, Destination, Host, KnownDevice, Media,
    MediaKey, NavCard, Nearby, Power, Prompt, RouteStage, Update,
};

#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    Persist(String, Value),
    ApplyAudioBuffer(u32),
    ConfigureAccessPoint(String, String),
    ConfigureRadio(Radio),
    AudioOutputs,
    Link,
    VolumeStep(i32),
    RequestResume,
    NightModeChanged,
    RequestPower(Power),
    CameraSelectInput(usize),
    CameraCycleStandard(i32),
    CameraAdjustLevel(usize, i32),
    SetCameraRunning(bool),
    SetMapRunning(bool),
    MapPan(i32, i32),
    MapZoom(i32),
    MapLocate,
    MapSearch(String),
    MapNearest(Nearby, usize),
    MapGo(usize),
    MapGoPlace(String),
    MapEndRoute,
    MapReroute,
    MapPreviewStart,
    MapPreviewCancel,
    SetKeyLearning(bool),
    BtPromptAccept,
    BtPromptCancel,
    BtScan(bool),
    BtPair(String),
    BtConnect(String, bool),
    BtForget(String),
    BtScreenVisible(bool),
    DeviceConnect(String),
    DeviceStop(String),
    DeviceForget(String),
    MediaKey(MediaKey),
    UpdateServe,
    UpdateScreens(bool),
    UpdateInstall,
    UpdateCancel,
    UpdateOutcomeSeen,
}

pub struct TestHost {
    pub settings: Settings,
    pub user: UserFile,
    pub calls: Vec<Call>,

    pub status: String,
    /// Milliseconds since local midnight; `None`: the clock is not set.
    pub local_time: Option<u64>,
    pub backgrounded: bool,
    pub link_usb: bool,
    pub link_wifi: bool,
    /// The sun is down: Night set to Auto is night.
    pub sun_down: bool,

    pub volume_level: u32,
    pub volume_muted: bool,
    pub volume_gen: u32,

    pub camera_present: bool,
    pub camera_inputs: usize,
    pub camera_input: usize,
    pub camera_no_signal: bool,
    pub camera_standard: Option<String>,
    pub camera_levels: Vec<CameraLevel>,
    pub camera_gen: u32,
    pub camera_failure: Option<String>,
    pub camera_request: CameraRequest,
    /// The map's guidance along a route, when it guides, and the speed.
    pub map_guide: Option<NavCard>,
    pub map_speed: Option<(u32, Option<u16>)>,
    /// Metres to the next speed camera on the route.
    pub map_camera: Option<u32>,
    /// Map > Go to: the names searched (a search keeps those whose name
    /// holds the text typed, any case), the recent ones (an empty search),
    /// what the last search found, and how the route goes: each poll
    /// takes the next stage queued (Finding while there is none).
    pub map_can_search: bool,
    pub map_names: Vec<Destination>,
    pub map_recent: Vec<Destination>,
    pub map_found: Vec<Destination>,
    /// The places of each kind there are, nearest first.
    pub map_nearby: Vec<(Nearby, Destination)>,
    pub map_stages: std::collections::VecDeque<RouteStage>,
    pub map_route_note: String,
    pub map_on_route: bool,
    pub map_rerouting: bool,
    /// A route shown whole (RouteStage::Preview).
    pub map_preview: Option<crate::host::RoutePreview>,
    /// The map was moved off where we are.
    pub map_away: bool,
    /// The regions on the card; the one `map.dir` names is current.
    pub map_regions: Vec<crate::host::MapRegion>,
    pub map_regions_gen: u32,
    pub map_voice: bool,
    /// The voices on the card; the one `map.voice-dir` names (else the
    /// first) is current. Empty: one English voice while `map_voice`.
    pub map_voices: Vec<crate::host::MapVoice>,
    /// Over the map while there is no picture.
    pub map_note: String,
    pub learned_key: Option<u32>,

    pub bt_available: bool,
    pub bt_gen: u32,
    pub bt_paired: Vec<BtDevice>,
    pub bt_found: Vec<BtDevice>,
    pub bt_summary: String,
    pub bt_status: String,
    pub bt_scanning: bool,
    pub bt_scan_allowed: bool,
    pub bt_phone: bool,
    pub bt_prompt: Prompt,

    pub devices: Vec<KnownDevice>,
    pub devices_gen: u32,

    pub media: Media,
    pub media_gen: u32,
    /// The art the screens take next, as `Host::take_art`.
    pub art: Option<Art>,
    pub nav: NavCard,
    pub nav_gen: u32,

    /// hostapd's radio; `None`: the config is missing.
    pub ap: Option<Radio>,
    /// Writing the config is refused (EACCES).
    pub ap_denied: bool,
    pub ap_applied: Applied,
    pub outputs: Vec<AudioOutput>,
    pub link: Link,
    pub names: crate::host::SystemNames,
    pub load: lehu_core::sysinfo::Load,
    pub video: u32,

    pub update: Update,
    pub update_gen: u32,
}

impl Default for TestHost {
    fn default() -> Self {
        Self::new()
    }
}

impl TestHost {
    pub fn new() -> Self {
        let paths = Paths {
            image: "/nonexistent/lehu/lehu.toml".into(),
            user: "/nonexistent/lehu/user.toml".into(),
        };
        let loaded = settings::load(&paths).expect("the defaults load");
        Self {
            settings: loaded.settings,
            user: loaded.user,
            calls: Vec::new(),
            status: String::new(),
            local_time: None,
            backgrounded: false,
            link_usb: false,
            link_wifi: false,
            sun_down: false,
            volume_level: 60,
            volume_muted: false,
            volume_gen: 1,
            camera_present: false,
            camera_inputs: 0,
            camera_input: 0,
            camera_no_signal: false,
            camera_standard: None,
            camera_levels: Vec::new(),
            camera_gen: 1,
            camera_failure: None,
            camera_request: CameraRequest::None,
            map_guide: None,
            map_speed: None,
            map_camera: None,
            map_can_search: false,
            map_names: Vec::new(),
            map_recent: Vec::new(),
            map_found: Vec::new(),
            map_nearby: Vec::new(),
            map_stages: std::collections::VecDeque::new(),
            map_route_note: String::new(),
            map_on_route: false,
            map_rerouting: false,
            map_preview: None,
            map_away: false,
            map_regions: Vec::new(),
            map_regions_gen: 0,
            map_voice: true,
            map_voices: Vec::new(),
            map_note: String::new(),
            learned_key: None,
            bt_available: true,
            bt_gen: 1,
            bt_paired: Vec::new(),
            bt_found: Vec::new(),
            bt_summary: String::new(),
            bt_status: String::new(),
            bt_scanning: false,
            bt_scan_allowed: true,
            bt_phone: false,
            bt_prompt: Prompt::default(),
            devices: Vec::new(),
            devices_gen: 1,
            media: Media::default(),
            media_gen: 1,
            art: None,
            nav: NavCard::default(),
            nav_gen: 1,
            ap: Some(Radio {
                band: Band::G2,
                channel: 6,
                width: 20,
                country: String::new(),
            }),
            ap_denied: false,
            ap_applied: Applied::Restarted,
            outputs: Vec::new(),
            link: Link::default(),
            names: crate::host::SystemNames {
                kernel: "6.18.42".into(),
                os: "Buildroot 2026.02".into(),
                lehu: "0.1.0".into(),
            },
            load: lehu_core::sysinfo::Load::default(),
            video: 0,
            update: Update {
                shown: true,
                available: true,
                version: "22.0.1-1".into(),
                ..Update::default()
            },
            update_gen: 1,
        }
    }

    fn ap_write(&self) -> Result<Applied, lehu_wifi::Error> {
        if self.ap.is_none() {
            return Err(lehu_wifi::Error::Read(std::io::ErrorKind::NotFound.into()));
        }
        if self.ap_denied {
            return Err(lehu_wifi::Error::Write(
                std::io::ErrorKind::PermissionDenied.into(),
            ));
        }
        Ok(self.ap_applied)
    }

    /// The settings in force from the defaults and the user layer, as
    /// `settings::load` builds them.
    fn rebuild(&mut self) {
        let mut table = Settings::defaults_table();
        settings::merge(&mut table, self.user.table().clone());
        self.settings = Value::Table(table)
            .try_into()
            .expect("the user layer holds valid settings");
    }

    /// The value persisted for `key`, if any.
    pub fn persisted(&self, key: &str) -> Option<&Value> {
        self.calls.iter().rev().find_map(|c| match c {
            Call::Persist(k, v) if k == key => Some(v),
            _ => None,
        })
    }

    pub fn count(&self, call: &Call) -> usize {
        self.calls.iter().filter(|c| *c == call).count()
    }

    pub fn has(&self, call: &Call) -> bool {
        self.count(call) > 0
    }
}

impl Host for TestHost {
    fn settings(&self) -> &Settings {
        &self.settings
    }

    fn persist(&mut self, key: &str, value: Value) -> Result<(), settings::Error> {
        self.calls
            .push(Call::Persist(key.to_owned(), value.clone()));
        self.user.set(key, value)?;
        self.rebuild();
        Ok(())
    }

    fn apply_audio_buffer(&mut self, ms: u32) {
        self.calls.push(Call::ApplyAudioBuffer(ms));
        self.settings.audio.buffer_ms = ms;
    }

    fn configure_access_point(
        &mut self,
        ssid: &str,
        passphrase: &str,
    ) -> Result<Applied, lehu_wifi::Error> {
        self.calls.push(Call::ConfigureAccessPoint(
            ssid.to_owned(),
            passphrase.to_owned(),
        ));
        lehu_wifi::check(ssid, passphrase)?;
        let applied = self.ap_write()?;
        self.settings.wifi.ssid = ssid.to_owned();
        self.settings.wifi.passphrase = passphrase.to_owned();
        Ok(applied)
    }

    fn access_point(&mut self) -> Result<Radio, lehu_wifi::Error> {
        self.ap
            .clone()
            .ok_or_else(|| lehu_wifi::Error::Read(std::io::ErrorKind::NotFound.into()))
    }

    fn configure_radio(&mut self, radio: &Radio) -> Result<Applied, lehu_wifi::Error> {
        self.calls.push(Call::ConfigureRadio(radio.clone()));
        lehu_wifi::check_radio(radio)?;
        let applied = self.ap_write()?;
        self.ap = Some(radio.clone());
        Ok(applied)
    }

    fn audio_outputs(&mut self) -> &[AudioOutput] {
        self.calls.push(Call::AudioOutputs);
        let device = self.settings.audio.device.clone();
        for o in &mut self.outputs {
            o.active = o.device == device;
        }
        &self.outputs
    }

    fn link(&mut self) -> Link {
        self.calls.push(Call::Link);
        self.link.clone()
    }

    fn volume_level(&self) -> u32 {
        self.volume_level
    }

    fn volume_muted(&self) -> bool {
        self.volume_muted
    }

    fn volume_generation(&self) -> u32 {
        self.volume_gen
    }

    fn volume_step(&mut self, direction: i32) {
        self.calls.push(Call::VolumeStep(direction));
        let level = self.volume_level as i32 + 5 * direction;
        self.volume_level = level.clamp(0, 100) as u32;
        self.volume_muted = false;
        self.volume_gen += 1;
    }

    fn status(&self) -> &str {
        &self.status
    }

    fn local_time_ms(&self) -> Option<u64> {
        self.local_time
    }

    fn session_backgrounded(&self) -> bool {
        self.backgrounded
    }

    fn link_usb(&self) -> bool {
        self.link_usb
    }

    fn link_wifi(&self) -> bool {
        self.link_wifi
    }

    fn request_resume(&mut self) {
        self.calls.push(Call::RequestResume);
    }

    fn night_mode_changed(&mut self) {
        self.calls.push(Call::NightModeChanged);
    }

    fn night_now(&self) -> bool {
        match self.settings.display.night_mode {
            settings::NightMode::Day => false,
            settings::NightMode::Night => true,
            settings::NightMode::Auto => self.sun_down,
        }
    }

    fn request_power(&mut self, action: Power) {
        self.calls.push(Call::RequestPower(action));
    }

    fn camera_present(&self) -> bool {
        self.camera_present
    }

    fn camera_inputs(&self) -> usize {
        self.camera_inputs
    }

    fn camera_input(&self) -> usize {
        self.camera_input
    }

    fn camera_no_signal(&self) -> bool {
        self.camera_no_signal
    }

    fn camera_has_standard(&self) -> bool {
        self.camera_standard.is_some()
    }

    fn camera_standard_label(&self) -> &str {
        self.camera_standard.as_deref().unwrap_or("")
    }

    fn camera_levels(&self) -> &[CameraLevel] {
        &self.camera_levels
    }

    fn camera_generation(&self) -> u32 {
        self.camera_gen
    }

    fn camera_select_input(&mut self, index: usize) {
        self.calls.push(Call::CameraSelectInput(index));
        self.camera_input = index;
        self.camera_gen += 1;
    }

    fn camera_cycle_standard(&mut self, direction: i32) {
        self.calls.push(Call::CameraCycleStandard(direction));
    }

    fn camera_adjust_level(&mut self, index: usize, direction: i32) {
        self.calls.push(Call::CameraAdjustLevel(index, direction));
    }

    fn set_camera_running(&mut self, on: bool) {
        self.calls.push(Call::SetCameraRunning(on));
    }

    fn set_map_running(&mut self, on: bool) {
        self.calls.push(Call::SetMapRunning(on));
    }

    fn map_pan(&mut self, dx: i32, dy: i32) {
        self.calls.push(Call::MapPan(dx, dy));
    }

    fn map_zoom(&mut self, step: i32) {
        self.calls.push(Call::MapZoom(step));
    }

    fn map_locate(&mut self) {
        self.calls.push(Call::MapLocate);
    }

    fn map_has_position(&self) -> bool {
        true
    }

    fn map_cursor_available(&self) -> bool {
        true
    }

    fn mouse_available(&self) -> bool {
        true
    }

    fn map_away(&self) -> bool {
        self.map_away
    }

    fn map_regions(&mut self) -> Vec<crate::host::MapRegion> {
        let dir = self.settings.map.dir.clone();
        self.map_regions
            .iter()
            .map(|r| crate::host::MapRegion {
                current: r.dir == dir,
                ..r.clone()
            })
            .collect()
    }

    fn map_regions_generation(&self) -> u32 {
        self.map_regions_gen
    }

    fn map_voice_installed(&self) -> bool {
        self.map_voice
    }

    fn map_voices(&mut self) -> Vec<crate::host::MapVoice> {
        if !self.map_voice {
            return Vec::new();
        }
        if self.map_voices.is_empty() {
            return vec![crate::host::MapVoice {
                dir: String::new(),
                name: "English".into(),
                current: true,
            }];
        }
        let dir = self.settings.map.voice_dir.clone();
        let named = self.map_voices.iter().any(|v| v.dir == dir);
        self.map_voices
            .iter()
            .enumerate()
            .map(|(i, v)| crate::host::MapVoice {
                current: if named { v.dir == dir } else { i == 0 },
                ..v.clone()
            })
            .collect()
    }

    fn map_guidance(&self) -> Option<&NavCard> {
        self.map_guide.as_ref()
    }

    fn map_speed(&self) -> Option<(u32, Option<u16>)> {
        self.map_speed
    }

    fn map_camera_ahead(&self, within: u32) -> Option<u32> {
        self.map_camera
            .filter(|&m| m <= within && self.settings.map.speed_cameras)
    }

    fn map_label(&self) -> &str {
        "Test  -  zoom 12"
    }

    fn map_note(&self) -> &str {
        &self.map_note
    }

    fn map_can_search(&self) -> bool {
        self.map_can_search
    }

    fn map_search(&mut self, query: &str) {
        self.calls.push(Call::MapSearch(query.to_string()));
        let q = query.to_lowercase();
        self.map_found = if q.is_empty() {
            self.map_recent.clone()
        } else {
            self.map_names
                .iter()
                .filter(|d| d.name.to_lowercase().contains(&q))
                .cloned()
                .collect()
        };
    }

    fn map_can_find(&self, kind: Nearby) -> bool {
        self.map_nearby.iter().any(|(k, _)| *k == kind)
    }

    fn map_nearest(&mut self, kind: Nearby, limit: usize) {
        self.calls.push(Call::MapNearest(kind, limit));
        self.map_found = self
            .map_nearby
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, d)| d.clone())
            .take(limit)
            .collect();
    }

    fn map_destinations(&self) -> &[Destination] {
        &self.map_found
    }

    fn map_go(&mut self, index: usize) {
        self.calls.push(Call::MapGo(index));
    }

    fn map_keep(&self, index: usize) -> Option<settings::MapPlace> {
        let d = self.map_found.get(index)?;
        Some(settings::MapPlace {
            name: d.name.clone(),
            town: d.detail.split(',').next().unwrap_or("").to_string(),
            at: "49.600000, 6.130000".into(),
        })
    }

    fn map_go_place(&mut self, place: &settings::MapPlace) {
        self.calls.push(Call::MapGoPlace(place.name.clone()));
    }

    fn map_place_distance(&self, _place: &settings::MapPlace) -> String {
        "2.4 km".into()
    }

    fn map_route_poll(&mut self) -> RouteStage {
        self.map_stages.pop_front().unwrap_or(RouteStage::Finding)
    }

    fn map_route_note(&self) -> &str {
        &self.map_route_note
    }

    fn map_on_route(&self) -> bool {
        self.map_on_route
    }
    fn map_rerouting(&self) -> bool {
        self.map_rerouting
    }

    fn map_end_route(&mut self) {
        self.calls.push(Call::MapEndRoute);
        self.map_on_route = false;
    }

    fn map_preview(&self) -> Option<&crate::host::RoutePreview> {
        self.map_preview.as_ref()
    }

    fn map_preview_start(&mut self) {
        self.calls.push(Call::MapPreviewStart);
        if self.map_preview.take().is_some() {
            self.map_on_route = true;
        }
    }

    fn map_preview_cancel(&mut self) {
        self.calls.push(Call::MapPreviewCancel);
        self.map_preview = None;
    }

    fn map_reroute(&mut self) -> bool {
        self.calls.push(Call::MapReroute);
        self.map_on_route
    }

    fn take_camera_failure(&mut self, why: &mut String) -> bool {
        match self.camera_failure.take() {
            Some(text) => {
                why.clear();
                why.push_str(&text);
                true
            }
            None => false,
        }
    }

    fn take_camera_request(&mut self) -> CameraRequest {
        std::mem::take(&mut self.camera_request)
    }

    fn set_key_learning(&mut self, on: bool) {
        self.calls.push(Call::SetKeyLearning(on));
        self.learned_key = None;
    }

    fn take_learned_key(&mut self) -> Option<u32> {
        self.learned_key.take()
    }

    fn bt_available(&self) -> bool {
        self.bt_available
    }

    fn bt_generation(&self) -> u32 {
        self.bt_gen
    }

    fn bt_paired(&self) -> &[BtDevice] {
        &self.bt_paired
    }

    fn bt_found(&self) -> &[BtDevice] {
        &self.bt_found
    }

    fn bt_summary(&self) -> &str {
        &self.bt_summary
    }

    fn bt_status(&self) -> &str {
        &self.bt_status
    }

    fn bt_scanning(&self) -> bool {
        self.bt_scanning
    }

    fn bt_scan_allowed(&self) -> bool {
        self.bt_scan_allowed
    }

    fn bt_phone_connected(&self) -> bool {
        self.bt_phone
    }

    fn bt_prompt(&self) -> &Prompt {
        &self.bt_prompt
    }

    fn bt_prompt_accept(&mut self) {
        self.calls.push(Call::BtPromptAccept);
        self.bt_prompt = Prompt::default();
        self.bt_gen += 1;
    }

    fn bt_prompt_cancel(&mut self) {
        self.calls.push(Call::BtPromptCancel);
        self.bt_prompt = Prompt::default();
        self.bt_gen += 1;
    }

    fn bt_scan(&mut self, on: bool) {
        self.calls.push(Call::BtScan(on));
    }

    fn bt_pair(&mut self, id: &str) {
        self.calls.push(Call::BtPair(id.to_owned()));
    }

    fn bt_connect(&mut self, id: &str, on: bool) {
        self.calls.push(Call::BtConnect(id.to_owned(), on));
    }

    fn bt_forget(&mut self, id: &str) {
        self.calls.push(Call::BtForget(id.to_owned()));
        self.bt_paired.retain(|d| d.id != id);
        self.bt_gen += 1;
    }

    fn bt_screen_visible(&mut self, visible: bool) {
        self.calls.push(Call::BtScreenVisible(visible));
    }

    fn devices(&self) -> &[KnownDevice] {
        &self.devices
    }

    fn devices_generation(&self) -> u32 {
        self.devices_gen
    }

    fn device_connect(&mut self, id: &str) {
        self.calls.push(Call::DeviceConnect(id.to_owned()));
    }

    fn device_stop(&mut self, id: &str) {
        self.calls.push(Call::DeviceStop(id.to_owned()));
    }

    fn device_forget(&mut self, id: &str) {
        self.calls.push(Call::DeviceForget(id.to_owned()));
        self.devices.retain(|d| d.id != id);
        self.devices_gen += 1;
    }

    fn media(&self) -> &Media {
        &self.media
    }

    fn media_generation(&self) -> u32 {
        self.media_gen
    }

    fn media_key(&mut self, key: MediaKey) {
        self.calls.push(Call::MediaKey(key));
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
        self.calls.push(Call::UpdateServe);
    }

    fn update_screens(&mut self, visible: bool) {
        self.calls.push(Call::UpdateScreens(visible));
    }

    fn update_install(&mut self) {
        self.calls.push(Call::UpdateInstall);
    }

    fn update_cancel(&mut self) {
        self.calls.push(Call::UpdateCancel);
    }

    fn system_names(&self) -> crate::host::SystemNames {
        self.names.clone()
    }

    fn system_load(&mut self) -> lehu_core::sysinfo::Load {
        self.load
    }

    fn video_bytes(&self) -> u32 {
        self.video
    }

    fn update_outcome_seen(&mut self) {
        self.calls.push(Call::UpdateOutcomeSeen);
        self.update.outcome = crate::host::UpdateOutcome::None;
        self.update_gen += 1;
    }
}
