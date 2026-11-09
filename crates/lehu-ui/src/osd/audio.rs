// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Settings > Audio (screen 6): each stream's level under the volume, in
//! steps of 10 % and applied as it is turned (the host's `persist` hands
//! the levels to the mixer at once); the music buffer (`pick`); and the
//! output, stepped through the playback devices the host lists and taken
//! with Enter. The program follows a new output at once: the mixer starts
//! again on it and the volume control moves to its card.

use lehu_core::settings::Settings;
use slint::SharedString;
use toml::Value;

use super::{Bridge, Host, State, clean, compose};

/// The streams' settings, in the rows' order.
const LEVEL_KEYS: [&str; 4] = [
    "audio.music-level",
    "audio.navigation-level",
    "audio.alert-level",
    "audio.call-level",
];

/// One step of a stream's level, in percent.
pub(super) const LEVEL_STEP: u32 = 10;

fn levels_of(s: &Settings) -> [u32; 4] {
    let a = &s.audio;
    [
        a.music_level,
        a.navigation_level,
        a.alert_level,
        a.call_level,
    ]
}

/// A level's next step: up to the next ten, down to the one before.
fn stepped(level: u32, direction: i32) -> u32 {
    let level = level.min(100);
    if direction > 0 {
        (level / LEVEL_STEP + 1) * LEVEL_STEP
    } else {
        level.div_ceil(LEVEL_STEP).saturating_sub(1) * LEVEL_STEP
    }
    .min(100)
}

impl<H: Host> State<H> {
    /// The levels' labels, on change.
    pub(super) fn pump_audio(&mut self, bridge: &Bridge<'_>) {
        let levels = levels_of(self.host.settings());
        if Some(levels) != self.p_levels {
            self.p_levels = Some(levels);
            for (i, level) in levels.into_iter().enumerate() {
                let label = if level == 0 {
                    SharedString::from("Off")
                } else {
                    SharedString::from(compose(&mut self.scratch, format_args!("{level}%")))
                };
                let level = level.min(100) as i32;
                match i {
                    0 => {
                        bridge.set_music_label(label);
                        bridge.set_music_level(level);
                    }
                    1 => {
                        bridge.set_navigation_label(label);
                        bridge.set_navigation_level(level);
                    }
                    2 => {
                        bridge.set_alert_label(label);
                        bridge.set_alert_level(level);
                    }
                    _ => {
                        bridge.set_call_label(label);
                        bridge.set_call_level(level);
                    }
                }
            }
        }
    }

    /// A stream's level one step up or down, applied and saved at once.
    pub(super) fn step_level(&mut self, stream: usize, direction: i32) {
        let Some(&key) = LEVEL_KEYS.get(stream) else {
            return;
        };
        let now = levels_of(self.host.settings())[stream];
        let next = stepped(now, direction);
        if next != now {
            self.persist(key, Value::Integer(i64::from(next)));
        }
        self.pick_last_ms = self.now();
    }

    /// The playback devices listed again (the screen opened, or the first
    /// look), and the output's label with them.
    pub(super) fn refresh_outputs(&mut self, bridge: &Bridge<'_>) {
        self.outputs.clear();
        self.outputs.extend_from_slice(self.host.audio_outputs());
        self.output_now = self.outputs.iter().position(|o| o.active);
        self.show_output(bridge);
    }

    /// The Output row's value: the card the setting opens, by the name the
    /// card gives itself -- never the setting's raw PCM name.
    fn show_output(&mut self, bridge: &Bridge<'_>) {
        let label = match self.output_now.and_then(|i| self.outputs.get(i)) {
            Some(o) => o.label.as_str(),
            None => unlisted_output(&self.host.settings().audio.device),
        };
        if self.p_output_label.update(label) {
            bridge.set_output_label(clean(&mut self.scratch, self.p_output_label.as_str()));
        }
    }

    /// The Output row starts on the device in use; nothing to step through
    /// without a card.
    pub(super) fn output_start(&mut self) -> Option<i32> {
        if self.outputs.is_empty() {
            return None;
        }
        self.pick_list.clear();
        self.pick_list
            .extend(self.outputs.iter().map(|o| o.label.clone()));
        Some(self.output_now.unwrap_or(0) as i32)
    }

    /// Enter on an output: it is set, and the sound moves to it. The
    /// microphone stays on the card it was on.
    pub(super) fn output_take(&mut self, taken: i32) {
        let Some(index) = usize::try_from(taken)
            .ok()
            .filter(|&i| i < self.outputs.len())
        else {
            return;
        };
        if self.output_now == Some(index) {
            return;
        }
        let audio = &self.host.settings().audio;
        let mic = audio.mic_device.is_empty().then(|| audio.device.clone());
        if let Some(mic) = mic {
            self.persist("audio.mic-device", Value::String(mic));
        }
        let device = self.outputs[index].device.clone();
        self.persist("audio.device", Value::String(device));
        self.output_now = Some(index);
        self.output_dirty = true;
    }

    /// After a take: the label follows on the next pump.
    pub(super) fn pump_output(&mut self, bridge: &Bridge<'_>) {
        if !self.outputs_seen {
            self.outputs_seen = true;
            self.refresh_outputs(bridge);
        } else if self.output_dirty {
            self.output_dirty = false;
            self.show_output(bridge);
        }
    }
}

/// What the Output row says when the card the setting names is not among
/// those listed: the system's default, or that it is missing.
fn unlisted_output(device: &str) -> &'static str {
    if device.is_empty() || device == "default" {
        "Default"
    } else {
        "Not found"
    }
}

#[cfg(test)]
mod tests {
    use super::stepped;

    #[test]
    fn levels_step_by_ten_and_snap() {
        assert_eq!(stepped(100, 1), 100);
        assert_eq!(stepped(100, -1), 90);
        assert_eq!(stepped(0, -1), 0);
        assert_eq!(stepped(0, 1), 10);
        assert_eq!(stepped(75, 1), 80);
        assert_eq!(stepped(75, -1), 70);
        assert_eq!(stepped(5, -1), 0);
        assert_eq!(stepped(250, -1), 90);
    }
}
