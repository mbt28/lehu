// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The Phones screen (screen 10): the phones the head unit knows, as the
//! host lists them -- the connected one first. Each row is picked in place
//! as the Settings rows are (`Pick::Device`, the phone's id beside it):
//! Connect (Stop on the phone the session runs on), Forget, asked first,
//! or Cancel. Nothing is edited here.

use slint::SharedString;

use super::rows::{Item, Listed, sync_rows};
use super::{Ask, Bridge, Host, Pick, State, clean, compose};
use crate::DeviceRow;
use crate::host::KnownDevice;

/// A phone that never said its name.
const UNNAMED: &str = "Phone";

impl Item for KnownDevice {
    fn id(&self) -> &str {
        &self.id
    }
}

impl Listed for DeviceRow {
    type Item = KnownDevice;

    fn from_item(device: &KnownDevice, scratch: &mut String) -> Self {
        DeviceRow {
            id: SharedString::from(device.id.as_str()),
            name: clean(scratch, name_of(device)),
            detail: clean(scratch, &device.detail),
            active: device.active,
            nearby: device.nearby,
        }
    }

    fn id(&self) -> &str {
        self.id.as_str()
    }
}

fn name_of(device: &KnownDevice) -> &str {
    if device.name.is_empty() {
        UNNAMED
    } else {
        &device.name
    }
}

impl<H: Host> State<H> {
    /// The host's list -> the rows, when its generation moves.
    pub(super) fn pump_devices(&mut self, bridge: &Bridge<'_>) {
        let generation = self.host.devices_generation();
        if self.devices_seen && generation == self.p_devices_gen {
            return;
        }
        self.devices_seen = true;
        self.p_devices_gen = generation;
        let ring = bridge.get_ring_row();
        if sync_rows(
            &self.devices,
            self.host.devices(),
            &mut self.scratch,
            ring.as_str(),
            "device:",
        ) {
            self.ring_gone(bridge);
        }
        // A list that changed under an open choice -- the phone connected,
        // was forgotten -- ends it: its options may not be the ones shown.
        if self.pick == Pick::Device {
            self.pick = Pick::None;
            self.show_pick(bridge);
        }
        // The Phones tile and row: how many, and the one in session.
        let known = self.host.devices().len();
        let summary = match known {
            0 => "None yet",
            n => compose(&mut self.scratch, format_args!("{n} known")),
        };
        if self.p_phones_summary.update(summary) {
            bridge.set_phones_summary(SharedString::from(self.p_phones_summary.as_str()));
        }
        let active = self
            .host
            .devices()
            .iter()
            .find(|d| d.active)
            .map_or("", name_of);
        if self.p_active_phone.update(active) {
            bridge.set_active_phone(clean(&mut self.scratch, self.p_active_phone.as_str()));
        }
    }

    /// Enter (or a tap) on a phone's row: opens its choice, or takes what
    /// it shows.
    pub(super) fn device_press(&mut self, bridge: &Bridge<'_>, id: &str) {
        if self.pick == Pick::Device && self.device_id == id {
            self.device_take(bridge);
            return;
        }
        let Some(active) = self
            .host
            .devices()
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.active)
        else {
            return;
        };
        self.pick = Pick::Device;
        self.device_id.clear();
        self.device_id.push_str(id);
        // A phone that is not connected opens on Connect, what the driver
        // most likely came for; the connected one on Cancel, so a double
        // press stops nothing.
        self.pick_choice = if active { 2 } else { 0 };
        self.show_pick(bridge);
    }

    /// How many options the picking phone's row shows: Connect (or Stop,
    /// for the phone the session runs on), Forget and Cancel.
    pub(super) fn device_options(&self) -> i32 {
        3
    }

    /// Enter on the phone's row while it picks: takes what it shows.
    pub(super) fn device_take(&mut self, bridge: &Bridge<'_>) {
        let taken = self.pick_choice;
        self.pick = Pick::None;
        let id = self.device_id.as_str();
        let Some(device) = self.host.devices().iter().find(|d| d.id == id) else {
            self.show_pick(bridge);
            return;
        };
        // Connect (Stop on the connected phone's row), Forget, Cancel.
        let option = taken;
        let active = device.active;
        if option == 1 {
            compose(
                &mut self.device_ask,
                format_args!("Forget {}?", name_of(device)),
            );
        }
        match option {
            0 if active => self.host.device_stop(&self.device_id),
            0 => self.host.device_connect(&self.device_id),
            // Asked first.
            1 => {
                self.system_ask = Ask::ForgetDevice;
                self.dialog_dirty = true;
            }
            _ => {}
        }
        self.show_pick(bridge);
    }
}
