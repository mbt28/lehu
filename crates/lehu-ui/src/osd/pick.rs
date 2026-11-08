//! Rows picked in place (`Bridge.pick`), built for the knob: on Settings,
//! Night (day, night, auto), Power (reboot or power off, each asked first,
//! or cancel: a knob has no Back) and Volume, whose level the knob turns;
//! on Audio, the streams' levels, Audio buffer and Output; on Video, the
//! frame rate; on Access point, Band, Width, Channel and Country; on the
//! Phones screen, a phone's row (`devices`). Turns and arrows move along
//! the options, stopping at the ends so a fast spin cannot land anywhere
//! unexpected; Enter takes the one shown, Back leaves. A level applies at
//! each step -- a fast turn of the knob moving a 0-100 % level
//! `FAST_PERCENT` a detent -- a list stepped through (Output, Channel,
//! Country) only when Enter takes it.

use lehu_core::settings::{BUFFER_MAX_MS, NightMode};
use slint::SharedString;
use toml::Value;

use super::{
    ACCESS_POINT, AUDIO, Ask, BUFFER_SAVE_MS, BUFFER_STEP_MS, Bridge, DEVICES, FAST_DETENT_MS,
    FAST_PERCENT, Host, SETTINGS, SYSTEM, State, VIDEO,
};
use crate::host::UpdateStage;
use crate::keys::OsdKey;

/// Which row is picking, as `Bridge.pick` numbers them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pick {
    None,
    Night,
    Power,
    Volume,
    Buffer,
    /// A phone's row on the Devices screen; `device_id` says which.
    Device,
    /// A stream's level: music, navigation, alerts, calls.
    Level(usize),
    Output,
    Band,
    Width,
    Channel,
    Country,
    /// The phone's frame rate (Video).
    Fps,
}

impl Pick {
    pub(super) fn from_row(row: i32) -> Self {
        match row {
            2 => Pick::Night,
            3 => Pick::Power,
            4 => Pick::Volume,
            5 => Pick::Buffer,
            7..=10 => Pick::Level((row - 7) as usize),
            11 => Pick::Output,
            12 => Pick::Band,
            13 => Pick::Width,
            14 => Pick::Channel,
            15 => Pick::Country,
            16 => Pick::Fps,
            _ => Pick::None,
        }
    }

    fn id(self) -> i32 {
        match self {
            Pick::None => 0,
            Pick::Night => 2,
            Pick::Power => 3,
            Pick::Volume => 4,
            Pick::Buffer => 5,
            Pick::Device => 6,
            Pick::Level(i) => 7 + i as i32,
            Pick::Output => 11,
            Pick::Band => 12,
            Pick::Width => 13,
            Pick::Channel => 14,
            Pick::Country => 15,
            Pick::Fps => 16,
        }
    }

    /// Stepped with - / + (a level, or a list shown one at a time) rather
    /// than chosen from options side by side.
    pub(super) fn steps(self) -> bool {
        matches!(
            self,
            Pick::Volume
                | Pick::Buffer
                | Pick::Level(_)
                | Pick::Output
                | Pick::Channel
                | Pick::Country
        )
    }

    /// Stepped through a list (`State::pick_list`).
    fn listed(self) -> bool {
        matches!(self, Pick::Output | Pick::Channel | Pick::Country)
    }

    /// A level from 0 to 100 %: turned fast, it moves further a detent.
    fn percent(self) -> bool {
        matches!(self, Pick::Volume | Pick::Level(_))
    }
}

pub(super) fn night_label(mode: NightMode) -> &'static str {
    match mode {
        NightMode::Day => "Day",
        NightMode::Night => "Night",
        NightMode::Auto => "Auto",
    }
}

/// The Night row's options, in order.
pub(super) fn night_index(mode: NightMode) -> i32 {
    match mode {
        NightMode::Day => 0,
        NightMode::Night => 1,
        NightMode::Auto => 2,
    }
}

fn night_of(index: i32) -> NightMode {
    match index {
        0 => NightMode::Day,
        1 => NightMode::Night,
        _ => NightMode::Auto,
    }
}

impl<H: Host> State<H> {
    pub(super) fn show_pick(&mut self, bridge: &Bridge<'_>) {
        bridge.set_pick(self.pick.id());
        bridge.set_pick_choice(self.pick_choice);
        let device = if self.pick == Pick::Device {
            self.device_id.as_str()
        } else {
            ""
        };
        if bridge.get_device_pick().as_str() != device {
            bridge.set_device_pick(SharedString::from(device));
        }
        let value = if self.pick.listed() {
            usize::try_from(self.pick_choice)
                .ok()
                .and_then(|i| self.pick_list.get(i))
                .map_or("", String::as_str)
        } else {
            ""
        };
        if bridge.get_pick_value().as_str() != value {
            bridge.set_pick_value(super::clean(&mut self.scratch, value));
        }
        self.pick_last_ms = self.now();
    }

    /// The screen the picking row is on: a pick ends off it, and only there
    /// are the keys its own.
    pub(super) fn pick_screen(&self) -> i32 {
        match self.pick {
            Pick::Device => DEVICES,
            Pick::Volume | Pick::Buffer | Pick::Level(_) | Pick::Output => AUDIO,
            Pick::Band | Pick::Width | Pick::Channel | Pick::Country => ACCESS_POINT,
            Pick::Night | Pick::Fps => VIDEO,
            Pick::Power => SYSTEM,
            _ => SETTINGS,
        }
    }

    /// How many options the picking row has side by side.
    fn options(&self) -> i32 {
        match self.pick {
            Pick::Device => self.device_options(),
            Pick::Band | Pick::Fps => 2,
            Pick::Width => self.ap_widths().len() as i32,
            _ if self.pick.listed() => self.pick_list.len() as i32,
            _ => 3,
        }
    }

    /// Enter (or a tap) on a picked row: starts it, or takes what it shows.
    pub(super) fn pick_press(&mut self, bridge: &Bridge<'_>, row: Pick) {
        if row == Pick::None {
            return;
        }
        // Going down now would cut an update off.
        if row == Pick::Power
            && matches!(
                self.host.update().stage,
                UpdateStage::Installing | UpdateStage::Restarting
            )
        {
            return;
        }
        // A phone's row starts from its own callback, with its id.
        if row == Pick::Device {
            self.device_take(bridge);
            return;
        }
        let night = self.host.settings().display.night_mode;
        if self.pick != row {
            // A list with nothing in it, or an access point whose config
            // could not be read, has nothing to pick: said, not started.
            let start = match row {
                Pick::Output => self.output_start(),
                Pick::Band | Pick::Width | Pick::Channel | Pick::Country => self.ap_start(row),
                Pick::Night => Some(night_index(night)),
                Pick::Fps => Some(i32::from(self.host.settings().android_auto.fps >= 60)),
                Pick::Power => Some(2), // Cancel: nothing goes down by accident
                _ => Some(0),
            };
            let Some(choice) = start else {
                return;
            };
            self.detent_dir = 0; // a fresh start is never a fast turn
            self.pick = row;
            self.pick_choice = choice;
        } else {
            let taken = self.pick_choice;
            self.pick = Pick::None;
            match row {
                Pick::Night => {
                    if taken != night_index(night) {
                        match Value::try_from(night_of(taken)) {
                            Ok(value) => {
                                self.persist("display.night-mode", value);
                                // A running session switches at once.
                                self.host.night_mode_changed();
                            }
                            Err(e) => log::error!("night mode: {e}"),
                        }
                    }
                }
                Pick::Power => {
                    if taken < 2 {
                        // Asked first.
                        self.system_ask = if taken == 0 {
                            Ask::Reboot
                        } else {
                            Ask::Shutdown
                        };
                        self.dialog_dirty = true;
                    }
                }
                Pick::Fps => {
                    let fps = self.host.settings().android_auto.fps;
                    let want = if taken == 1 { 60 } else { 30 };
                    if (fps >= 60) != (want == 60) {
                        self.persist("android-auto.fps", Value::Integer(want));
                    }
                }
                Pick::Buffer => self.save_buffer(),
                Pick::Output => self.output_take(taken),
                Pick::Band | Pick::Width | Pick::Channel | Pick::Country => {
                    self.ap_take(row, taken)
                }
                // The levels: each step applied at once.
                Pick::Volume | Pick::Level(_) | Pick::None | Pick::Device => {}
            }
        }
        self.show_pick(bridge);
    }

    pub(super) fn pick_key(&mut self, bridge: &Bridge<'_>, nav: OsdKey, repeat: bool) {
        let level = self.pick.steps();
        let direction = match nav {
            OsdKey::Next | OsdKey::Right => 1,
            OsdKey::Prev | OsdKey::Left => -1,
            // Up is more on a level, back along the options elsewhere.
            OsdKey::Up => {
                if level {
                    1
                } else {
                    -1
                }
            }
            OsdKey::Down => {
                if level {
                    -1
                } else {
                    1
                }
            }
            OsdKey::Ok => {
                if !repeat {
                    let pick = self.pick;
                    self.pick_press(bridge, pick);
                }
                return;
            }
            OsdKey::Back | OsdKey::Erase => {
                if repeat {
                    return;
                }
                self.pick = Pick::None;
                self.show_pick(bridge);
                return;
            }
            _ => return,
        };
        // A knob turned fast moves a level further: detents close together
        // in the same direction. Arrows and touch always step once.
        let mut steps = 1;
        if matches!(nav, OsdKey::Next | OsdKey::Prev) && self.pick.percent() {
            let now = self.now();
            if self.detent_dir == direction && now.saturating_sub(self.detent_ms) < FAST_DETENT_MS {
                steps = self.fast_steps();
            }
            self.detent_ms = now;
            self.detent_dir = direction;
        }
        for _ in 0..steps {
            self.pick_step(bridge, direction);
        }
    }

    /// How many of the picking level's steps make `FAST_PERCENT`.
    fn fast_steps(&self) -> u32 {
        let step = match self.pick {
            Pick::Volume => self.host.settings().audio.volume_step,
            _ => super::audio::LEVEL_STEP,
        };
        (FAST_PERCENT / step.max(1)).max(1)
    }

    /// One step of the picking row, from a turn, an arrow or a - / + tap.
    pub(super) fn pick_step(&mut self, bridge: &Bridge<'_>, direction: i32) {
        match self.pick {
            Pick::None => return,
            Pick::Volume => self.host.volume_step(direction),
            Pick::Buffer => self.step_buffer(direction),
            Pick::Level(i) => self.step_level(i, direction),
            _ => {
                let last = (self.options() - 1).max(0);
                self.pick_choice = (self.pick_choice + direction).clamp(0, last);
            }
        }
        self.show_pick(bridge);
    }

    /// The music buffer one step deeper (+1) or shallower (-1). The mixer
    /// follows the setting at once; the file is written once turning stops.
    pub(super) fn step_buffer(&mut self, direction: i32) {
        let now_ms = self.host.settings().audio.buffer_ms as i32;
        let next =
            ((now_ms / BUFFER_STEP_MS + direction) * BUFFER_STEP_MS).clamp(0, BUFFER_MAX_MS as i32);
        if next == now_ms {
            return;
        }
        self.host.apply_audio_buffer(next as u32);
        let now = self.now();
        self.buffer_save_at = now + BUFFER_SAVE_MS;
        self.pick_last_ms = now;
    }

    pub(super) fn save_buffer(&mut self) {
        if self.buffer_save_at == 0 {
            return;
        }
        self.buffer_save_at = 0;
        let ms = self.host.settings().audio.buffer_ms;
        self.persist("audio.buffer-ms", Value::Integer(i64::from(ms)));
    }
}
