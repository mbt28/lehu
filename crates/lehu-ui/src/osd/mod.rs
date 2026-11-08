//! The controller behind the screens: what FastCarPlay's slint_osd did.
//!
//! It owns the RGB565 frame Slint renders into and hands the rows that
//! changed to a [`Surface`] (the DRM overlay plane). Touch and keys arrive
//! from any thread through [`Input`] and are dispatched here, on the loop
//! thread, because Slint is single-threaded. Everything else it needs goes
//! through [`Host`].
//!
//! The screens' own state lives in Slint (`Bridge.screen`, the focus ring,
//! what a row is picking); what has to outlive a screen -- the edit buffer,
//! the camera column's page, whose question the dialog shows -- lives here.
//! The pump writes a property only when its value changed: an unconditional
//! write per tick costs real CPU on this core. The UI's callbacks do not act
//! themselves; they queue an [`Action`] that is applied right after the
//! event that caused it, so nothing re-enters the controller while it is
//! dispatching into Slint.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::rc::Rc;

use lehu_core::settings::NightMode;
use slint::platform::software_renderer::{MinimalSoftwareWindow, PhysicalRegion, Rgb565Pixel};
use slint::platform::{Key, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, SharedString, VecModel};
use toml::Value;

use crate::host::{AudioOutput, CameraRequest, Host};
use crate::input::{Input, KEY_QUEUE, KeyEvent};
use crate::{
    AppWindow, Bridge, BtRow, CameraPanel, CameraRow, DeviceRow, NavCardBox, VolumeBar, platform,
    text,
};

mod audio;
mod bluetooth;
mod camera;
mod devices;
mod dialog;
mod editor;
mod info;
mod keys;
mod media;
mod pick;
mod pump;
mod rows;
#[cfg(test)]
mod shots;
#[cfg(test)]
mod tests;
mod update;
mod wireless;

use camera::CamRef;
use dialog::{Ask, DialogText};
use pick::Pick;
use wireless::ApWrite;

/// Where the rendered rows go: the overlay plane's buffer.
pub trait Surface {
    /// Rows `y0..y1` of the frame changed; `pixels` holds exactly those rows,
    /// full width, top row first. The destination is never read back.
    fn write_rows(&mut self, y0: u32, y1: u32, pixels: &[Rgb565Pixel]);
}

/// A window of the overlay plane, in display pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

// Screens, as Bridge.screen numbers them.
const HOME: i32 = 0;
const SETTINGS: i32 = 1;
const WIRELESS: i32 = 3;
const BLUETOOTH: i32 = 4;
const BT_DEVICE: i32 = 5;
const AUDIO: i32 = 6;
const VIDEO: i32 = 8;
const CAMERA: i32 = 9;
const DEVICES: i32 = 10;
const MEDIA: i32 = 11;
const ACCESS_POINT: i32 = 12;
const LINK: i32 = 13;
const UPDATE: i32 = 14;
const UPDATE_PHONE: i32 = 15;
const CONNECTION: i32 = 16;
const SYSTEM: i32 = 17;
const INFO: i32 = 18;
/// One past the highest screen number: the ring's memory has a place each.
const SCREENS: usize = 19;

/// The camera view's settings column hides this long after the last input.
const CAMERA_PANEL_MS: u64 = 5000;
/// The plane is held for the camera's first frame at most this long (the
/// loop releases it sooner, when the picture is up).
const HOLD_MAX_MS: u64 = 2000;
/// Keyboard focus is only re-seeded for someone actually using a keyboard.
const KEY_RECENT_MS: u64 = 10_000;
/// A row picked in place stops this long after the last input.
const PICK_MS: u64 = 10_000;
/// The music buffer's steps, and how long after the last step it is saved
/// (flash writes).
const BUFFER_STEP_MS: i32 = 50;
const BUFFER_SAVE_MS: u64 = 2000;
/// The volume bar stays up this long after the last change.
const VOLUME_BAR_MS: u64 = 2000;
/// The navigation card stays up this long after the last guidance update.
const NAV_CARD_MS: u64 = 30_000;
/// Knob detents closer than this on a level are a fast turn: each moves
/// the level `FAST_PERCENT` rather than one step.
const FAST_DETENT_MS: u64 = 120;
const FAST_PERCENT: u32 = 20;
/// The clock is looked at again this often while the system time is not
/// set.
const CLOCK_UNSET_MS: u64 = 10_000;

/// What the UI asked for, from its callbacks. Applied right after the event
/// that raised it, in order.
#[derive(Clone, Debug, PartialEq)]
enum Action {
    Back,
    PickPress(i32),
    PickOption(i32),
    PickStep(i32),
    Resume,
    VolumeStep(i32),
    BufferStep(i32),
    CameraSelect(i32),
    CameraAdjust(i32),
    CameraPicture,
    CameraKey,
    CameraFastswitchToggle,
    EditField(i32),
    OskKey(SharedString),
    OskBackspace,
    OskOk,
    OskCancel,
    BtScan(bool),
    BtPair(SharedString),
    BtOpen(SharedString),
    BtConnect(SharedString, bool),
    BtForget(SharedString),
    DevicePress(SharedString),
    MediaKey(i32),
    UpdateInstall,
    UpdateCancel,
    PromptAccept,
    PromptCancel,
}

/// More than this between two drains can only be a runaway; the newest are
/// dropped rather than replayed.
const ACTIONS: usize = 32;

type Actions = Rc<RefCell<VecDeque<Action>>>;

/// The screens and their controller.
pub struct Osd<H: Host> {
    ui: AppWindow,
    win: Rc<MinimalSoftwareWindow>,
    /// The complete current frame; the renderer repaints only what changed.
    buffer: Vec<Rgb565Pixel>,
    width: u32,
    height: u32,
    input: Input,
    actions: Actions,
    /// The keys of one tick, copied out of the shared queue.
    keys: [KeyEvent; KEY_QUEUE],
    st: State<H>,
}

/// The last value pushed for a text property, so an unchanged tick writes
/// nothing. The copy is kept in place: no allocation once it has grown.
#[derive(Default)]
struct Last(Option<String>);

impl Last {
    /// Remember `now`; true when it differs from what was remembered.
    fn update(&mut self, now: &str) -> bool {
        match &mut self.0 {
            Some(last) if last == now => false,
            Some(last) => {
                last.clear();
                last.push_str(now);
                true
            }
            None => {
                self.0 = Some(now.to_owned());
                true
            }
        }
    }

    fn as_str(&self) -> &str {
        self.0.as_deref().unwrap_or("")
    }
}

/// Everything the controller keeps between ticks. The methods that drive the
/// screens are on it, spread over the sibling modules, and take the Slint
/// handles as arguments, so the owner's borrows never fight.
struct State<H> {
    host: H,
    /// Added to the clock; tests move time with it.
    skew: u64,

    // ── rendering ───────────────────────────────────────────────────────
    /// Push the whole frame next (the first frame, a backend transition).
    full_push: bool,
    /// The camera view coming up: the plane keeps the screen it has until
    /// the camera's first frame is under it (`release_held`); what is drawn
    /// meanwhile -- rows held_y0..held_y1 -- is pushed then.
    held: bool,
    held_since: u64,
    held_y0: u32,
    held_y1: u32,
    last_down: bool,

    // ── editor ──────────────────────────────────────────────────────────
    /// The field being edited, -1 none; the buffer is owned here, the UI
    /// only displays it.
    editing: i32,
    edit_buffer: String,

    // ── last pushed values ──────────────────────────────────────────────
    p_status: Last,
    /// The session's link, as the phone's tile names it.
    p_link_label: &'static str,
    /// The status, the waiting session or the link changed: the home
    /// tiles' words are composed again.
    home_dirty: bool,
    p_phone_title: Last,
    p_phone_detail: Last,
    p_status_note: Last,
    p_night: Option<NightMode>,
    /// The clock as shown, minutes since midnight (`None` not known), and
    /// when to look at the time again.
    p_clock: Option<Option<u32>>,
    clock_due: u64,
    p_fps: Option<u32>,
    p_ssid: Last,
    p_pass: Last,
    p_bt_name: Last,
    volume_seen: bool,
    p_volume_gen: u32,
    /// The bar is up (in the scene).
    volume_bar: bool,
    volume_bar_until: u64,
    p_link_usb: bool,
    p_link_wifi: bool,
    p_waiting: bool,

    // ── keyboard ────────────────────────────────────────────────────────
    // Switching a screen or closing a dialog destroys the focused row, so
    // focus is re-seeded on the first row -- but only for someone who used
    // a key recently (touch never shows a ring).
    last_key_ms: u64,
    key_used: bool,
    focus_seed: bool,
    p_screen: i32,
    p_modal: bool,
    /// The held Backspace was pressed in the editor.
    erase_edits: bool,
    /// The row with the ring went away while its screen stayed: next tick,
    /// once it is really gone, the ring goes back on the first row.
    ring_lost: bool,
    /// This pump saw a screen switch.
    switched: bool,
    /// Where the ring was on each screen when it was last left: going back
    /// to a screen puts it there again.
    ring_memory: [String; SCREENS],
    /// The row the next seeding looks for; `None`: the first one.
    ring_target: Option<String>,
    /// Where Back from the Phones screen goes: Home or Settings, whichever
    /// opened it.
    devices_parent: i32,

    // ── rows picked in place ────────────────────────────────────────────
    pick: Pick,
    /// The option shown as taken.
    pick_choice: i32,
    pick_last_ms: u64,
    /// The last knob detent on a level, and its direction: a fast turn
    /// moves further.
    detent_ms: u64,
    detent_dir: i32,
    p_buffer_ms: Option<u32>,
    /// When the music buffer is written; 0: saved.
    buffer_save_at: u64,

    // ── the camera view's column ────────────────────────────────────────
    panel_up: bool,
    panel_until: u64,
    /// 0 the menu, 1 Picture.
    cam_page: i32,
    /// The ring; -1: the menu's last row (Picture).
    cam_row: i32,
    /// The ringed level is being turned.
    cam_editing: bool,
    p_cam_page: i32,
    p_cam_row: i32,
    p_cam_editing: bool,
    /// The page changed: rebuild its rows.
    cam_rows_dirty: bool,
    /// The screen the view goes back to.
    cam_return: i32,
    /// Switched on by a request: no column at first.
    cam_quiet: bool,
    p_camera_key: Option<u32>,
    p_fast_switch: Option<bool>,
    /// A touch that only brought the column back.
    touch_swallow: bool,
    /// What each row of the page is.
    cam_items: Vec<CamRef>,
    /// Set once and never replaced (see `bluetooth::sync_rows`).
    cam_rows: Rc<VecModel<CameraRow>>,
    cam_seen: bool,
    p_cam_gen: u32,

    // ── the dialog ──────────────────────────────────────────────────────
    /// Bluetooth's prompt, as last seen.
    bt_dialog: DialogText,
    /// Our own question.
    system_ask: Ask,
    /// Either changed: `pump_dialog` looks again.
    dialog_dirty: bool,
    /// The dialog shown is our own question.
    dialog_local: bool,
    p_prompt_kind: i32,
    p_prompt_title: Last,
    p_prompt_code: Last,
    p_prompt_hint: Last,
    p_prompt_accept: Last,

    // ── bluetooth ───────────────────────────────────────────────────────
    bt_paired: Rc<VecModel<BtRow>>,
    bt_found: Rc<VecModel<BtRow>>,
    /// First snapshot pushed.
    bt_seen: bool,
    p_bt_gen: u32,
    /// The device screen's device (by id).
    bt_current_id: String,
    p_bt_current: BtRow,
    p_bt_summary: Last,
    p_bt_status: Last,
    p_bt_scanning: bool,
    p_bt_scan_allowed: bool,
    p_link_bt: bool,
    p_bt_available: bool,

    // ── devices ─────────────────────────────────────────────────────────
    /// Set once and never replaced (see `rows::sync_rows`).
    devices: Rc<VecModel<DeviceRow>>,
    devices_seen: bool,
    p_devices_gen: u32,
    p_phones_summary: Last,
    p_active_phone: Last,
    /// The phone picking in place, or asked about (by id).
    device_id: String,
    /// The Forget question's title ("Forget <name>?").
    device_ask: String,

    // ── now playing and the navigation card ─────────────────────────────
    media_seen: bool,
    p_media_gen: u32,
    p_media_available: bool,
    /// The second the position shows; `None`: write it again.
    p_media_second: Option<u32>,
    /// The progress bar as shown, in thousandths (-1 none).
    p_media_progress: i32,
    nav_seen: bool,
    p_nav_gen: u32,
    /// The card may show until then; 0 once guidance stopped.
    nav_until: u64,
    p_nav_card: bool,
    /// The last tick had only a window up and the camera not in view: the
    /// phone's video is in front.
    over_phone: bool,

    // ── Settings > Audio ────────────────────────────────────────────────
    p_levels: Option<[u32; 4]>,
    /// The playback devices as listed when the screen last opened.
    outputs: Vec<AudioOutput>,
    outputs_seen: bool,
    /// The one `audio.device` opens, or was just set to.
    output_now: Option<usize>,
    output_dirty: bool,
    p_output_label: Last,
    /// The list a row steps through (Output, Channel, Country), as shown.
    pick_list: Vec<String>,

    // ── the access point and the link ───────────────────────────────────
    /// hostapd's radio, read when its page (or Link speed) opened.
    ap: Option<lehu_wifi::Radio>,
    /// The page's header: why it cannot change, or how the change went.
    ap_status: String,
    p_ap_status: Last,
    p_ap: Option<lehu_wifi::Radio>,
    /// A change waiting for the frame that says "Restarting...".
    ap_pending: Option<ApWrite>,
    ap_armed: bool,
    /// The Link speed page's next sample, and the counters of the last.
    link_due: u64,
    info_due: u64,
    info_prev: Option<info::Sample>,
    p_info: [Last; 5],
    link_prev: Option<(u64, u64, u64)>,
    p_link: [Last; 6],

    // ── Settings > Update ───────────────────────────────────────────────
    update_seen: bool,
    p_update_gen: u32,
    p_update_stage: crate::host::UpdateStage,
    /// The program was last told the Update pages show (or not).
    p_update_tree: Option<bool>,
    /// What the QR codes on show say.
    p_qr_join: Last,
    p_qr_page: Last,
    /// The last update's outcome was put up (once per run).
    update_asked: bool,
    /// The outcome's answer opens the Update page, next pump.
    update_goto: bool,
    /// The outcome dialog's text.
    update_hint: String,

    // ── scratch ─────────────────────────────────────────────────────────
    /// Text being composed or sanitised for a property.
    scratch: String,
    /// The camera's failure text.
    why: String,
}

impl<H: Host> State<H> {
    fn new(host: H) -> Self {
        Self {
            host,
            skew: 0,
            full_push: true,
            held: false,
            held_since: 0,
            held_y0: 0,
            held_y1: 0,
            last_down: false,
            editing: -1,
            edit_buffer: String::with_capacity(64),
            p_status: Last::default(),
            p_link_label: "",
            home_dirty: true,
            p_phone_title: Last::default(),
            p_phone_detail: Last::default(),
            p_status_note: Last::default(),
            p_night: None,
            p_clock: None,
            clock_due: 0,
            p_fps: None,
            p_ssid: Last::default(),
            p_pass: Last::default(),
            p_bt_name: Last::default(),
            volume_seen: false,
            p_volume_gen: 0,
            volume_bar: false,
            volume_bar_until: 0,
            p_link_usb: false,
            p_link_wifi: false,
            p_waiting: false,
            last_key_ms: 0,
            key_used: false,
            focus_seed: false,
            p_screen: -1,
            p_modal: false,
            erase_edits: false,
            ring_lost: false,
            switched: false,
            ring_memory: Default::default(),
            ring_target: None,
            devices_parent: CONNECTION,
            pick: Pick::None,
            pick_choice: 0,
            pick_last_ms: 0,
            detent_ms: 0,
            detent_dir: 0,
            p_buffer_ms: None,
            buffer_save_at: 0,
            panel_up: false,
            panel_until: 0,
            cam_page: 0,
            cam_row: 0,
            cam_editing: false,
            p_cam_page: -1,
            p_cam_row: -1,
            p_cam_editing: false,
            cam_rows_dirty: true,
            cam_return: VIDEO,
            cam_quiet: false,
            p_camera_key: None,
            p_fast_switch: None,
            touch_swallow: false,
            cam_items: Vec::with_capacity(12),
            cam_rows: Rc::new(VecModel::default()),
            cam_seen: false,
            p_cam_gen: 0,
            bt_dialog: DialogText::default(),
            system_ask: Ask::None,
            dialog_dirty: true,
            dialog_local: false,
            p_prompt_kind: 0,
            p_prompt_title: Last::default(),
            p_prompt_code: Last::default(),
            p_prompt_hint: Last::default(),
            p_prompt_accept: Last::default(),
            bt_paired: Rc::new(VecModel::default()),
            bt_found: Rc::new(VecModel::default()),
            bt_seen: false,
            p_bt_gen: 0,
            bt_current_id: String::new(),
            p_bt_current: BtRow::default(),
            p_bt_summary: Last::default(),
            p_bt_status: Last::default(),
            p_bt_scanning: false,
            p_bt_scan_allowed: true,
            p_link_bt: false,
            p_bt_available: false,
            devices: Rc::new(VecModel::default()),
            devices_seen: false,
            p_devices_gen: 0,
            p_phones_summary: Last::default(),
            p_active_phone: Last::default(),
            device_id: String::new(),
            device_ask: String::new(),
            media_seen: false,
            p_media_gen: 0,
            p_media_available: false,
            p_media_second: None,
            p_media_progress: -2,
            nav_seen: false,
            p_nav_gen: 0,
            nav_until: 0,
            p_nav_card: false,
            over_phone: false,
            p_levels: None,
            outputs: Vec::new(),
            outputs_seen: false,
            output_now: None,
            output_dirty: false,
            p_output_label: Last::default(),
            pick_list: Vec::new(),
            ap: None,
            ap_status: String::new(),
            p_ap_status: Last::default(),
            p_ap: None,
            ap_pending: None,
            ap_armed: false,
            link_due: 0,
            info_due: 0,
            info_prev: None,
            p_info: Default::default(),
            link_prev: None,
            p_link: Default::default(),
            update_seen: false,
            p_update_gen: 0,
            p_update_stage: crate::host::UpdateStage::Idle,
            p_update_tree: None,
            p_qr_join: Last::default(),
            p_qr_page: Last::default(),
            update_asked: false,
            update_goto: false,
            update_hint: String::new(),
            scratch: String::with_capacity(128),
            why: String::new(),
        }
    }

    /// The program's clock, in milliseconds.
    fn now(&self) -> u64 {
        lehu_core::clock::now_ms() + self.skew
    }

    /// Set a setting. Every setting applies at once: the program follows
    /// the settings' generation.
    fn persist(&mut self, key: &str, value: Value) {
        if let Err(e) = self.host.persist(key, value) {
            log::error!("could not save {key}: {e}");
        }
    }

    /// Someone is using keys (a keyboard, a remote, the knob): the ring is
    /// shown for them. Touch never shows it.
    fn keys_recent(&self) -> bool {
        self.key_used && self.now().saturating_sub(self.last_key_ms) < KEY_RECENT_MS
    }

    /// The screen Back goes to from `screen`; `None` at home.
    fn parent(&self, screen: i32) -> Option<i32> {
        match screen {
            HOME => None,
            BT_DEVICE => Some(BLUETOOTH),
            BLUETOOTH | ACCESS_POINT | LINK => Some(WIRELESS),
            WIRELESS => Some(CONNECTION),
            UPDATE | INFO => Some(SYSTEM),
            UPDATE_PHONE => Some(UPDATE),
            CAMERA => Some(self.cam_return),
            DEVICES => Some(self.devices_parent),
            MEDIA | SETTINGS => Some(HOME),
            _ => Some(SETTINGS),
        }
    }

    /// `to` is reached from `from` by going back: Back, the Home key, a
    /// page closing on its own.
    fn goes_back(&self, from: i32, to: i32) -> bool {
        let mut at = from;
        // The chain is short; the bound only guards against a loop.
        for _ in 0..SCREENS {
            match self.parent(at) {
                Some(p) if p == to => return true,
                Some(p) if p != at => at = p,
                _ => return false,
            }
        }
        false
    }

    /// The ring's row on the screen shown, remembered before a dialog or
    /// the editor covers it: the screen goes before the switch is seen.
    fn remember_ring(&mut self, bridge: &Bridge<'_>) {
        let ring = bridge.get_ring_row();
        if let Some(slot) = usize::try_from(bridge.get_screen())
            .ok()
            .and_then(|i| self.ring_memory.get_mut(i))
            && !ring.is_empty()
        {
            slot.clear();
            slot.push_str(ring.as_str());
        }
    }

    /// One level up from the screen shown; false at home.
    fn back(&mut self, bridge: &Bridge<'_>) -> bool {
        match self.parent(bridge.get_screen()) {
            Some(parent) => {
                bridge.set_screen(parent);
                true
            }
            None => false,
        }
    }

    /// The ring's row is gone (or going): off it now; and unless this pump
    /// saw a screen switch (which re-seeds anyway), back on the first row
    /// next tick, once the row is really destroyed.
    fn ring_gone(&mut self, bridge: &Bridge<'_>) {
        bridge.set_ring_row(SharedString::default());
        if !self.switched {
            self.ring_lost = true;
        }
    }

    /// The UI asked for something (a callback fired inside a dispatch).
    fn act(&mut self, ui: &AppWindow, action: Action) {
        let bridge = ui.global::<Bridge<'_>>();
        match action {
            Action::Back => {
                self.back(&bridge);
            }
            Action::PickPress(row) => self.pick_press(&bridge, Pick::from_row(row)),
            Action::PickOption(option) => {
                if self.pick == Pick::None || self.pick.steps() {
                    return;
                }
                self.pick_choice = option;
                let pick = self.pick;
                self.pick_press(&bridge, pick);
            }
            // - / + by touch on the row that is stepping.
            Action::PickStep(direction) => self.pick_step(&bridge, direction),
            Action::Resume => self.host.request_resume(),
            // Applies at once (no restart) and saves itself a moment later;
            // the level row stays up while used.
            Action::VolumeStep(direction) => {
                self.host.volume_step(direction);
                self.pick_last_ms = self.now();
            }
            Action::BufferStep(direction) => self.step_buffer(direction),
            // The camera view's column, by touch; any of it keeps the
            // column up.
            Action::CameraSelect(row) => {
                self.show_panel();
                self.cam_row = row;
                self.cam_editing = false;
                self.camera_press(&bridge, row, false);
            }
            Action::CameraAdjust(direction) => {
                self.show_panel();
                if let Some(index) = self.ringed_level() {
                    self.host.camera_adjust_level(index, direction);
                }
            }
            Action::CameraPicture => self.hide_panel(),
            // The next key on a Bluetooth keyboard is learned, not acted on.
            Action::CameraKey => {
                self.system_ask = Ask::CameraKey;
                self.dialog_dirty = true;
                self.host.set_key_learning(true);
            }
            Action::CameraFastswitchToggle => {
                let on = self.host.settings().camera.fast_switch;
                self.persist("camera.fast-switch", Value::Boolean(!on));
            }
            Action::EditField(id) => {
                if (0..editor::FIELD_COUNT).contains(&id) {
                    self.set_editing(ui, id);
                }
            }
            Action::OskKey(key) => self.osk_key(&bridge, key.as_str()),
            Action::OskBackspace => self.osk_backspace(&bridge),
            Action::OskOk => self.save_field(ui),
            Action::OskCancel => self.set_editing(ui, -1),
            Action::BtScan(on) => self.host.bt_scan(on),
            Action::BtPair(id) => self.host.bt_pair(id.as_str()),
            Action::BtOpen(id) => {
                self.bt_current_id.clear();
                self.bt_current_id.push_str(id.as_str());
                self.bt_seen = false; // re-pump bt-current now
            }
            Action::BtConnect(id, on) => self.host.bt_connect(id.as_str(), on),
            Action::BtForget(id) => self.host.bt_forget(id.as_str()),
            Action::DevicePress(id) => self.device_press(&bridge, id.as_str()),
            Action::MediaKey(key) => self.media_key(key),
            // Asked first: the head unit restarts.
            Action::UpdateInstall => {
                if self.host.update().stage == crate::host::UpdateStage::Ready {
                    self.system_ask = Ask::Install;
                    self.dialog_dirty = true;
                }
            }
            Action::UpdateCancel => {
                if self.host.update().can_cancel {
                    self.system_ask = Ask::CancelUpdate;
                    self.dialog_dirty = true;
                }
            }
            // The dialog answers whoever asked: our own question, or Bluetooth.
            Action::PromptAccept => {
                if self.dialog_local {
                    self.answer_system(true);
                } else {
                    self.host.bt_prompt_accept();
                }
            }
            Action::PromptCancel => {
                if self.dialog_local {
                    self.answer_system(false);
                } else {
                    self.host.bt_prompt_cancel();
                }
            }
        }
    }
}

/// `raw` reduced to the embedded glyphs, as a Slint string.
fn clean(scratch: &mut String, raw: &str) -> SharedString {
    text::sanitize_into(scratch, raw);
    SharedString::from(scratch.as_str())
}

/// The ring on the first stop of the screen, then on to `target` if it is
/// there: Tab walks the chain until the ring reports that row, or comes
/// round to the first again.
fn seed_ring(ui: &AppWindow, target: Option<&str>) {
    dispatch_key(ui, Key::Tab, false);
    let Some(target) = target else {
        return;
    };
    let bridge = ui.global::<Bridge<'_>>();
    let first = bridge.get_ring_row();
    if first.is_empty() {
        return;
    }
    // A screen has far fewer stops; the bound only guards a chain that
    // never comes round.
    for _ in 0..64 {
        if bridge.get_ring_row().as_str() == target {
            return;
        }
        dispatch_key(ui, Key::Tab, false);
        if bridge.get_ring_row() == first {
            return;
        }
    }
}

/// Tab, Backtab or Return as Slint key events: they reach the focused row.
fn dispatch_key(ui: &AppWindow, key: Key, repeat: bool) {
    let text: SharedString = key.into();
    let window = ui.window();
    if repeat {
        window.dispatch_event(WindowEvent::KeyPressRepeated { text: text.clone() });
    } else {
        window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    }
    window.dispatch_event(WindowEvent::KeyReleased { text });
}

fn queue(actions: &RefCell<VecDeque<Action>>, action: Action) {
    let mut actions = actions.borrow_mut();
    if actions.len() < ACTIONS {
        actions.push_back(action);
    } else {
        log::warn!("screens: action queue full, {action:?} dropped");
    }
}

/// Apply what the UI asked for during the last dispatch, in order. Applying
/// one never dispatches into Slint, so this cannot recurse.
fn drain<H: Host>(actions: &Actions, st: &mut State<H>, ui: &AppWindow) {
    loop {
        let next = actions.borrow_mut().pop_front();
        match next {
            Some(action) => st.act(ui, action),
            None => break,
        }
    }
}

/// The UI's callbacks, each queueing its action.
fn wire(ui: &AppWindow, actions: &Actions) {
    let bridge = ui.global::<Bridge<'_>>();
    macro_rules! on {
        ($name:ident, || $action:expr) => {{
            let q = actions.clone();
            bridge.$name(move || queue(&q, $action));
        }};
        ($name:ident, |$($arg:ident),+| $action:expr) => {{
            let q = actions.clone();
            bridge.$name(move |$($arg),+| queue(&q, $action));
        }};
    }
    on!(on_back, || Action::Back);
    on!(on_pick_press, |row| Action::PickPress(row));
    on!(on_pick_option, |option| Action::PickOption(option));
    on!(on_pick_step, |direction| Action::PickStep(direction));
    on!(on_resume, || Action::Resume);
    on!(on_volume_step, |direction| Action::VolumeStep(direction));
    on!(on_buffer_step, |direction| Action::BufferStep(direction));
    on!(on_camera_select, |row| Action::CameraSelect(row));
    on!(on_camera_adjust, |direction| Action::CameraAdjust(
        direction
    ));
    on!(on_camera_picture, || Action::CameraPicture);
    on!(on_camera_key, || Action::CameraKey);
    on!(on_camera_fastswitch_toggle, || {
        Action::CameraFastswitchToggle
    });
    on!(on_edit_field, |id| Action::EditField(id));
    on!(on_osk_key, |key| Action::OskKey(key));
    on!(on_osk_backspace, || Action::OskBackspace);
    on!(on_osk_ok, || Action::OskOk);
    on!(on_osk_cancel, || Action::OskCancel);
    on!(on_bt_scan, |on| Action::BtScan(on));
    on!(on_bt_pair, |id| Action::BtPair(id));
    on!(on_bt_open, |id| Action::BtOpen(id));
    on!(on_bt_connect, |id, on| Action::BtConnect(id, on));
    on!(on_bt_forget, |id| Action::BtForget(id));
    on!(on_device_press, |id| Action::DevicePress(id));
    on!(on_media_key, |key| Action::MediaKey(key));
    on!(on_update_install, || Action::UpdateInstall);
    on!(on_update_cancel, || Action::UpdateCancel);
    on!(on_prompt_accept, || Action::PromptAccept);
    on!(on_prompt_cancel, || Action::PromptCancel);
}

impl<H: Host> Osd<H> {
    /// The screens, `width` x `height` pixels (the panel's size, from the
    /// display, never a constant), driving `host`. Slint's platform is set
    /// for this thread here, once per process.
    pub fn new(host: H, width: u32, height: u32) -> Result<Self, slint::PlatformError> {
        if width == 0 || height == 0 {
            return Err(slint::PlatformError::Other(
                "the screens need a size".into(),
            ));
        }
        let (ui, win) = platform::create(width, height)?;
        let actions: Actions = Rc::new(RefCell::new(VecDeque::with_capacity(ACTIONS)));
        let st = State::new(host);
        wire(&ui, &actions);
        {
            let bridge = ui.global::<Bridge<'_>>();
            bridge.set_editing(-1);
            bridge.set_camera_rows(ModelRc::from(st.cam_rows.clone()));
            bridge.set_bt_paired(ModelRc::from(st.bt_paired.clone()));
            bridge.set_bt_found(ModelRc::from(st.bt_found.clone()));
            bridge.set_devices(ModelRc::from(st.devices.clone()));
        }
        let pixels = width as usize * height as usize;
        log::info!(
            "screens: {width}x{height} RGB565, {} KB frame",
            (pixels * 2) / 1024
        );
        Ok(Self {
            ui,
            win,
            buffer: vec![Rgb565Pixel::default(); pixels],
            width,
            height,
            input: Input::new(),
            actions,
            keys: [KeyEvent::default(); KEY_QUEUE],
            st,
        })
    }

    /// The input side, for the touch and key threads.
    pub fn input(&self) -> Input {
        self.input.clone()
    }

    /// The touch point, in display pixels (see [`Input::pointer`]).
    pub fn pointer(&self, x: i32, y: i32, pressed: bool) {
        self.input.pointer(x, y, pressed);
    }

    /// A key (see [`Input::key`]).
    pub fn key(&self, nav: crate::keys::OsdKey, ch: Option<char>, value: i32) {
        self.input.key(nav, ch, value);
    }

    /// The program as the screens see it: the loop updates it through here.
    pub fn host(&self) -> &H {
        &self.st.host
    }

    pub fn host_mut(&mut self) -> &mut H {
        &mut self.st.host
    }

    /// The screens exist and render; there is no state in which they do not
    /// once `new` succeeded, so this is for callers that used to ask.
    pub fn active(&self) -> bool {
        true
    }

    /// A camera switch asked from outside the screens (the camera key): shows
    /// or leaves the camera view. Every loop turn, whatever is shown.
    pub fn camera_requests(&mut self) {
        let bridge = self.ui.global::<Bridge<'_>>();
        let showing = bridge.get_screen() == CAMERA;
        let want = match self.st.host.take_camera_request() {
            CameraRequest::None => return,
            CameraRequest::Toggle => !showing,
            CameraRequest::On => true,
            CameraRequest::Off => false,
        };
        if want == showing {
            return;
        }
        if want {
            self.st.cam_quiet = true;
            bridge.set_screen(CAMERA);
        } else {
            bridge.set_screen(self.st.cam_return);
        }
        // From this turn on the loop runs (or stops) the camera, also over
        // phone video, where the screens are not ticked to see the change.
        self.st.host.set_camera_running(want);
    }

    /// The UI was just hidden (video took the screen): queued keys are
    /// stale, and what only runs while a screen is visible (discovery)
    /// stops.
    pub fn hidden(&mut self) {
        self.input.clear_keys();
        // Discovery stops with the screens. An open Bluetooth dialog stays:
        // it is still there to finish when the UI comes back, and BlueZ's
        // own timeouts end it otherwise.
        if self.st.p_screen == BLUETOOTH || self.st.p_screen == BT_DEVICE {
            self.st.host.bt_screen_visible(false);
        }
        // Our own reboot / shut down question does not wait behind the
        // video: the screens come back without it. How the last update
        // went does: it is said once.
        if self.st.system_ask != Ask::None && self.st.system_ask != Ask::UpdateOutcome {
            self.st.answer_system(false);
        }
        // p_screen stays: the screen -- and its focused row -- are still
        // there when the UI comes back, so there is nothing to re-seed.
    }

    /// Pumps the program's state into the UI, dispatches input, runs timers
    /// and animations and renders what changed. True when rows were written
    /// to the surface and the caller should show them. `over_video`: only a
    /// window of the plane is up (see [`over_video`](Self::over_video)) -- a
    /// whole-frame push is left for the screens.
    pub fn tick(&mut self, over_video: bool, surface: &mut dyn Surface) -> bool {
        // The ring's row was removed last tick and is destroyed by now: from
        // nothing focused, Tab lands on the first row.
        if self.st.ring_lost {
            self.st.ring_lost = false;
            dispatch_key(&self.ui, Key::Tab, false);
        }

        self.st.over_phone = over_video && self.st.p_screen != CAMERA;
        self.pointer_events();
        drain(&self.actions, &mut self.st, &self.ui);

        // Keys before the pump, so a screen a key switches to is pumped and
        // laid out in this same tick.
        let count = self.input.take_keys(&mut self.keys);
        for key in &self.keys[..count] {
            self.st.apply_key(&self.ui, *key);
            drain(&self.actions, &mut self.st, &self.ui);
        }

        self.st.pump(&self.ui);
        if self.st.focus_seed {
            self.st.focus_seed = false;
            let target = self.st.ring_target.take();
            // Tab from nothing focused lands on the first stop (Back): the
            // ring is where a keyboard user expects it after a switch --
            // or, back on a screen, on the row that was left from.
            if self.st.keys_recent() {
                seed_ring(&self.ui, target.as_deref());
            }
        }
        slint::platform::update_timers_and_animations();

        self.render(over_video, surface)
    }

    /// Touch -> pointer events, on this thread as Slint requires. A tap that
    /// fit entirely between two ticks still delivers a press + release pair.
    fn pointer_events(&mut self) {
        let p = self.input.take_pointer();
        let position = LogicalPosition::new(p.x as f32, p.y as f32);
        let window = self.ui.window();
        let st = &mut self.st;
        // The camera view with its column down: a touch only brings the
        // column back, the tap itself goes nowhere. Any touch keeps it up.
        let began = !st.last_down && (p.down || p.seen);
        if began && st.p_screen == CAMERA {
            if !st.panel_up {
                st.touch_swallow = true;
            }
            st.show_panel();
        }
        let button = PointerEventButton::Left;
        if st.touch_swallow {
            if !p.down {
                st.touch_swallow = false;
            }
        } else if p.down && !st.last_down {
            window.dispatch_event(WindowEvent::PointerPressed { position, button });
        } else if !p.down && st.last_down {
            window.dispatch_event(WindowEvent::PointerReleased { position, button });
        } else if p.down {
            window.dispatch_event(WindowEvent::PointerMoved { position });
        } else if p.seen {
            window.dispatch_event(WindowEvent::PointerPressed { position, button });
            window.dispatch_event(WindowEvent::PointerReleased { position, button });
        }
        st.last_down = p.down;
    }

    /// Render what changed into the frame and push the changed rows.
    fn render(&mut self, over_video: bool, surface: &mut dyn Surface) -> bool {
        let st = &mut self.st;
        let now = st.now();
        // Held for the camera, at most this long and only on its view.
        if st.held && (st.p_screen != CAMERA || now - st.held_since >= HOLD_MAX_MS) {
            st.held = false;
        }
        let flush_held = !st.held && st.held_y1 > st.held_y0;
        // Over video only the bar's window is up: a pending whole-frame
        // push waits for the screens (copying 750 KB here would be wasted).
        let full_push = st.full_push && !over_video;

        // The renderer runs when something is dirty or animating; a whole
        // push or a flush of held rows runs it too, so the frame is current.
        if full_push || flush_held || self.ui.window().has_active_animations() {
            WindowAdapter::request_redraw(&*self.win);
        }
        let width = self.width as usize;
        let buffer = &mut self.buffer;
        let mut region: Option<PhysicalRegion> = None;
        let drawn = self.win.draw_if_needed(|renderer| {
            region = Some(renderer.render(buffer, width));
        });
        if !drawn {
            return false;
        }
        if full_push {
            st.full_push = false;
        }

        let (mut y0, mut y1) = if full_push {
            (0, self.height)
        } else {
            band(region.as_ref(), self.height)
        };
        if st.held {
            if st.held_y1 <= st.held_y0 {
                st.held_y0 = y0;
                st.held_y1 = y1;
            } else if y1 > y0 {
                st.held_y0 = st.held_y0.min(y0);
                st.held_y1 = st.held_y1.max(y1);
            }
            return false;
        }
        if flush_held {
            if y1 <= y0 {
                y0 = st.held_y0;
                y1 = st.held_y1;
            } else {
                y0 = y0.min(st.held_y0);
                y1 = y1.max(st.held_y1);
            }
            st.held_y0 = 0;
            st.held_y1 = 0;
        }
        if y1 <= y0 {
            return false; // nothing actually changed
        }
        let rows = &self.buffer[y0 as usize * width..y1 as usize * width];
        surface.write_rows(y0, y1, rows);
        true
    }

    /// What to show over live video right now -- the camera view's settings
    /// column while it is up, else the volume bar for a moment after a
    /// volume change (also one not ticked in yet), else the navigation card
    /// over the camera's picture -- and where: the overlay plane's window,
    /// one rectangle, so the three take turns, the card last. Cheap enough
    /// to ask every loop turn.
    pub fn over_video(&self) -> Option<Rect> {
        let st = &self.st;
        // The column: the volume bar then sits inside it, at its foot.
        if st.p_screen == CAMERA && st.panel_up {
            let panel = self.ui.global::<CameraPanel<'_>>();
            return Some(rect(
                panel.get_x(),
                panel.get_y(),
                panel.get_width(),
                panel.get_height(),
            ));
        }
        // A change the next tick has not pumped yet brings the bar up too.
        let pending = st.volume_seen && st.host.volume_generation() != st.p_volume_gen;
        if !pending && !(st.volume_bar && st.now() < st.volume_bar_until) {
            // Over the phone's video the card never shows: the phone draws
            // its own route.
            if st.p_screen == CAMERA && st.p_nav_card && st.now() < st.nav_until {
                let card = self.ui.global::<NavCardBox<'_>>();
                return Some(rect(
                    card.get_x(),
                    card.get_y(),
                    card.get_width(),
                    card.get_height(),
                ));
            }
            return None;
        }
        let bar = self.ui.global::<VolumeBar<'_>>();
        Some(rect(
            bar.get_x(),
            bar.get_y(),
            bar.get_width(),
            bar.get_height(),
        ))
    }

    /// How long until the screens want a tick without input: the volume bar
    /// or the navigation card going, the Media page's position line moving
    /// on, the Link speed page's next sample. With Slint's own timers, the
    /// loop's poll timeout.
    pub fn next_due(&self) -> Option<std::time::Duration> {
        let st = &self.st;
        let now = st.now();
        [
            st.volume_bar.then_some(st.volume_bar_until),
            (st.nav_until > now).then_some(st.nav_until),
            st.media_due(now),
            st.link_due(),
            st.info_due(),
            // The clock's next minute, while it shows the time.
            matches!(st.p_clock, Some(Some(_))).then_some(st.clock_due),
        ]
        .into_iter()
        .flatten()
        .min()
        .map(|at| std::time::Duration::from_millis(at.saturating_sub(now)))
    }

    /// Push the whole frame to the surface on the next tick (a backend
    /// transition: the frame always holds the complete current picture).
    pub fn invalidate(&mut self) {
        self.st.full_push = true;
        WindowAdapter::request_redraw(&*self.win);
    }

    /// The camera's picture is up: what the view drew meanwhile goes to the
    /// surface on the next tick.
    pub fn release_held(&mut self) {
        self.st.held = false;
    }
}

impl<H: Host> Drop for Osd<H> {
    fn drop(&mut self) {
        // The platform itself stays: Slint cannot unset it within a thread.
        if let Err(e) = self.ui.hide() {
            log::debug!("screens: hide: {e}");
        }
    }
}

/// The rows a render touched, clamped to the frame; empty as (height, 0).
fn band(region: Option<&PhysicalRegion>, height: u32) -> (u32, u32) {
    let Some(region) = region else {
        return (height, 0);
    };
    let size = region.bounding_box_size();
    if size.width == 0 || size.height == 0 {
        return (height, 0);
    }
    let y0 = region.bounding_box_origin().y.max(0) as u32;
    (y0.min(height), (y0 + size.height).min(height))
}

fn rect(x: f32, y: f32, width: f32, height: f32) -> Rect {
    Rect {
        x: x.max(0.0) as u32,
        y: y.max(0.0) as u32,
        width: width.max(0.0) as u32,
        height: height.max(0.0) as u32,
    }
}

/// Write formatted text into `out`, cleared first (a String never fails).
fn compose<'a>(out: &'a mut String, args: std::fmt::Arguments<'_>) -> &'a str {
    out.clear();
    let _ = out.write_fmt(args);
    out.as_str()
}
