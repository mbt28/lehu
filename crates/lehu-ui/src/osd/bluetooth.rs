//! Bluetooth's state -> the Bridge, gated on the manager's generation: an
//! idle screen pays one load per tick. The device lists are synced in place
//! by id (`rows`); the manager's prompt feeds the dialog.

use slint::SharedString;

use super::rows::{Item, Listed, sync_rows};
use super::{BLUETOOTH, BT_DEVICE, Bridge, Host, State, clean};
use crate::BtRow;
use crate::host::BtDevice;
use crate::osd::dialog::prompt_kind_id;

impl Item for BtDevice {
    fn id(&self) -> &str {
        &self.id
    }
}

impl Listed for BtRow {
    type Item = BtDevice;

    fn from_item(device: &BtDevice, scratch: &mut String) -> Self {
        BtRow {
            id: SharedString::from(device.id.as_str()),
            name: clean(scratch, &device.name),
            detail: clean(scratch, &device.detail),
            connected: device.connected,
        }
    }

    fn id(&self) -> &str {
        self.id.as_str()
    }
}

impl<H: Host> State<H> {
    pub(super) fn pump_bluetooth(&mut self, bridge: &Bridge<'_>, screen: i32) {
        let generation = self.host.bt_generation();
        if self.bt_seen && generation == self.p_bt_gen {
            return;
        }
        self.bt_seen = true;
        self.p_bt_gen = generation;

        // Bluetooth can come up after the screens do: its row then appears.
        let available = self.host.bt_available();
        if available != self.p_bt_available {
            self.p_bt_available = available;
            bridge.set_bt_available(available);
        }

        let ring = bridge.get_ring_row();
        let lost = sync_rows(
            &self.bt_paired,
            self.host.bt_paired(),
            &mut self.scratch,
            ring.as_str(),
            "paired:",
        ) | sync_rows(
            &self.bt_found,
            self.host.bt_found(),
            &mut self.scratch,
            ring.as_str(),
            "found:",
        );
        if lost {
            self.ring_gone(bridge);
        }

        let current = self
            .host
            .bt_paired()
            .iter()
            .find(|d| d.id == self.bt_current_id);
        let present = current.is_some();
        let current =
            current.map_or_else(BtRow::default, |d| BtRow::from_item(d, &mut self.scratch));
        if current != self.p_bt_current {
            bridge.set_bt_current(current.clone());
            self.p_bt_current = current;
        }
        // The device screen's device was forgotten: back to the list.
        if screen == BT_DEVICE && !present {
            bridge.set_screen(BLUETOOTH);
        }

        if self.p_bt_summary.update(self.host.bt_summary()) {
            bridge.set_bt_summary(clean(&mut self.scratch, self.p_bt_summary.as_str()));
        }
        if self.p_bt_status.update(self.host.bt_status()) {
            bridge.set_bt_status(clean(&mut self.scratch, self.p_bt_status.as_str()));
        }
        let scanning = self.host.bt_scanning();
        if scanning != self.p_bt_scanning {
            self.p_bt_scanning = scanning;
            bridge.set_bt_scanning(scanning);
        }
        let scan_allowed = self.host.bt_scan_allowed();
        if scan_allowed != self.p_bt_scan_allowed {
            self.p_bt_scan_allowed = scan_allowed;
            bridge.set_bt_scan_allowed(scan_allowed);
        }
        let link_bt = self.host.bt_phone_connected();
        if link_bt != self.p_link_bt {
            self.p_link_bt = link_bt;
            bridge.set_link_bt(link_bt);
        }

        let prompt = self.host.bt_prompt();
        self.bt_dialog.kind = prompt_kind_id(prompt.kind);
        self.bt_dialog.title.clone_from(&prompt.title);
        self.bt_dialog.code.clone_from(&prompt.code);
        self.bt_dialog.hint.clone_from(&prompt.hint);
        self.bt_dialog.accept.clone_from(&prompt.accept);
        self.dialog_dirty = true;
    }
}
