// SPDX-License-Identifier: GPL-3.0-or-later
//! The Wireless screen's fields -- the access point's name and passphrase,
//! the Bluetooth name -- each edited full screen with the on-screen
//! keyboard. The buffer is owned here; the UI only displays it. WPA2 fixes
//! the passphrase at 8-63 characters and the SSID at 1-32; hostapd refuses
//! to start outside those, so they are enforced before saving.

use lehu_core::settings::Settings;
use slint::{ComponentHandle, SharedString};
use toml::Value;

use super::{Bridge, Host, State, clean, compose};
use crate::{AppWindow, Osk};

pub(super) struct Field {
    /// The setting, for the fields persisted as one.
    key: &'static str,
    /// The editor's title.
    label: &'static str,
    /// Bytes, as hostapd counts them.
    min: usize,
    max: usize,
}

const FIELDS: [Field; 3] = [
    Field {
        key: "wifi.ssid",
        label: "Name",
        min: 1,
        max: 32,
    },
    Field {
        key: "wifi.passphrase",
        label: "Password",
        min: 8,
        max: 63,
    },
    Field {
        key: "bluetooth.name",
        label: "Bluetooth name",
        min: 1,
        max: 32,
    },
];
pub(super) const FIELD_COUNT: i32 = FIELDS.len() as i32;

fn field_value(settings: &Settings, id: i32) -> &str {
    match id {
        0 => &settings.wifi.ssid,
        1 => &settings.wifi.passphrase,
        2 => &settings.bluetooth.name,
        _ => "",
    }
}

impl<H: Host> State<H> {
    fn field(&self) -> Option<&'static Field> {
        usize::try_from(self.editing)
            .ok()
            .and_then(|i| FIELDS.get(i))
    }

    /// Open the editor on a field (-1 closes it).
    pub(super) fn set_editing(&mut self, ui: &AppWindow, id: i32) {
        let bridge = ui.global::<Bridge<'_>>();
        if self.editing < 0 && id >= 0 {
            self.remember_ring(&bridge);
        }
        self.editing = id;
        bridge.set_editing(id);
        if let Some(field) = self.field() {
            self.edit_buffer.clear();
            self.edit_buffer
                .push_str(field_value(self.host.settings(), id));
            bridge.set_editor_label(SharedString::from(field.label));
            bridge.set_editor_text(clean(&mut self.scratch, &self.edit_buffer));
            bridge.set_editor_error(SharedString::default());
            ui.global::<Osk<'_>>().invoke_reset(); // letters, no ring
        }
    }

    /// A key of the on-screen keyboard (or typed): appended within the limit.
    pub(super) fn osk_key(&mut self, bridge: &Bridge<'_>, key: &str) {
        let Some(field) = self.field() else {
            return;
        };
        if self.edit_buffer.len() + key.len() > field.max {
            return;
        }
        self.edit_buffer.push_str(key);
        bridge.set_editor_text(clean(&mut self.scratch, &self.edit_buffer));
    }

    /// Drop the last character (not the last byte: an existing value may
    /// hold multi-byte text even though the keyboard only emits ASCII).
    pub(super) fn osk_backspace(&mut self, bridge: &Bridge<'_>) {
        if self.field().is_none() {
            return;
        }
        self.edit_buffer.pop();
        bridge.set_editor_text(clean(&mut self.scratch, &self.edit_buffer));
    }

    /// OK: validate, save, close -- or say what is wrong and stay open
    /// rather than save something unusable.
    pub(super) fn save_field(&mut self, ui: &AppWindow) {
        let Some(field) = self.field() else {
            return;
        };
        let bridge = ui.global::<Bridge<'_>>();
        let length = self.edit_buffer.len();
        if length < field.min || length > field.max {
            let message = compose(
                &mut self.scratch,
                format_args!("Needs {}-{} characters", field.min, field.max),
            );
            bridge.set_editor_error(SharedString::from(message));
            return;
        }
        if self.editing <= 1 {
            // The AP belongs to the system: written into its config and the
            // AP restarts, once the page under the editor says so. No
            // restart of ours needed -- the backends read the effective
            // config when a session starts.
            let settings = self.host.settings();
            let (ssid, pass) = if self.editing == 0 {
                (self.edit_buffer.clone(), settings.wifi.passphrase.clone())
            } else {
                (settings.wifi.ssid.clone(), self.edit_buffer.clone())
            };
            self.ap_request(super::ApWrite::Network(ssid, pass));
        } else {
            // The Bluetooth name: the program renames the adapter at once.
            self.persist(field.key, Value::String(self.edit_buffer.clone()));
        }
        self.set_editing(ui, -1);
    }
}
