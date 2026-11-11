// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Map > Go to (screen 20): a destination by name, and the route to it.
//!
//! The screen opens on the recent destinations when there are some, else
//! on the keyboard. Each key typed searches again (the program's search
//! takes a few milliseconds), and the two best matches show over the
//! keyboard, a tap away; the keyboard's OK (List) shows them all as rows,
//! which the ring walks like any list. A tap on one asks the program for
//! the route, found on a thread of its own: the screen says it is looking,
//! goes back to the map once the route is there (the map then drives it),
//! or says why there is none. Back and Cancel go back to the map.
//!
//! Under the search row, the kinds of place there are to find the nearest
//! of (fuel, parking, charging): one chosen lists the nearest few in the
//! recent ones' place, each with a few words and how far, picked as any
//! destination; Back (or choosing it again) shows the recent ones again.
//!
//! The same screen chooses Home or Work ("Set Home"), from the map's menu
//! or from the Change rows under the recent destinations: a pick then
//! keeps the place, says so on the map's bar, and goes back to the map
//! without a route.

use slint::{ComponentHandle, Model, SharedString};
use toml::Value;

use super::map::FLASH_MS;
use super::{Bridge, DEST, Host, MAP, State, clean};
use crate::host::{MapSlot, Nearby, RouteStage};
use crate::{AppWindow, DestRow, Osk};

/// What can be typed: a long street name and its town.
const QUERY_MAX: usize = 48;
/// Over the keyboard: the best few.
const TOP: usize = 2;
/// The nearest of a kind: a screen and a bit, the nearest within reach
/// of a turn or two of the knob.
const NEARBY: usize = 8;

impl<H: Host> State<H> {
    /// The keyboard's keys are Go to's: its screen is up and no editor is.
    pub(super) fn dest_keys(&self, bridge: &Bridge<'_>) -> bool {
        self.editing < 0 && bridge.get_screen() == DEST
    }

    /// The screen opens: nothing typed; the recent destinations as a list
    /// when there are some (or Home and Work to change), else the
    /// keyboard. A route still being found stays so.
    pub(super) fn open_dest(&mut self, ui: &AppWindow) {
        self.dest_query.clear();
        self.dest_error = false;
        self.host.map_search("");
        let bridge = ui.global::<Bridge<'_>>();
        let title = match self.dest_choose {
            Some(MapSlot::Home) => "Set Home",
            Some(MapSlot::Work) => "Set Work",
            None => "Go to",
        };
        bridge.set_dest_title(SharedString::from(title));
        bridge.set_dest_choosing(self.dest_choose.is_some());
        self.dest_near = None;
        bridge.set_dest_near(0);
        let can = Nearby::ALL.map(|kind| self.host.map_can_find(kind));
        bridge.set_dest_can_fuel(can[0]);
        bridge.set_dest_can_parking(can[1]);
        bridge.set_dest_can_charging(can[2]);
        let map = &self.host.settings().map;
        let choosing = self.dest_choose.is_some();
        let changes = !choosing && (map.home.is_some() || map.work.is_some());
        let near = !choosing && can.contains(&true);
        let typing = self.host.map_destinations().is_empty() && !changes && !near;
        // A fresh list either way.
        self.dest_typing = !typing;
        self.show_dest(ui, typing);
    }

    /// Go to's Change Home (0) or Change Work (1): the screen anew, to
    /// choose it.
    pub(super) fn dest_change(&mut self, ui: &AppWindow, slot: i32) {
        if self.dest_busy || !self.dest_keys(&ui.global::<Bridge<'_>>()) {
            return;
        }
        self.dest_choose = Some(if slot == 0 {
            MapSlot::Home
        } else {
            MapSlot::Work
        });
        self.open_dest(ui);
    }

    /// The keyboard (`typing`) or the list, and everything on them anew.
    fn show_dest(&mut self, ui: &AppWindow, typing: bool) {
        let bridge = ui.global::<Bridge<'_>>();
        if typing && !self.dest_typing {
            ui.global::<Osk<'_>>().invoke_reset(); // letters, no ring
        }
        if typing && self.dest_near.take().is_some() {
            bridge.set_dest_near(0);
        }
        if typing != self.dest_typing {
            // A new set of stops for the ring.
            self.focus_seed = true;
            self.switched = true;
            bridge.set_ring_row(SharedString::default());
        }
        self.dest_typing = typing;
        bridge.set_dest_typing(typing);
        self.show_query(&bridge);
        self.p_dests = None;
        self.pump_dest(&bridge);
    }

    /// A kind of place chosen: its nearest listed; chosen again, the
    /// recent ones again.
    pub(super) fn dest_nearby(&mut self, bridge: &Bridge<'_>, id: i32) {
        let Some(kind) = Nearby::from_id(id) else {
            return;
        };
        if self.dest_busy || self.dest_typing || !self.dest_keys(bridge) {
            return;
        }
        if self.dest_near == Some(kind) {
            self.dest_near_close(bridge);
            return;
        }
        self.dest_near = Some(kind);
        self.dest_error = false;
        bridge.set_dest_near(kind.id());
        self.host.map_nearest(kind, NEARBY);
        self.pump_dest(bridge);
    }

    /// Back on the nearest of a kind: the recent ones again, true; false
    /// when they are not listed.
    pub(super) fn dest_near_close(&mut self, bridge: &Bridge<'_>) -> bool {
        if self.dest_near.is_none() || self.dest_busy {
            return false;
        }
        self.dest_near = None;
        self.dest_error = false;
        bridge.set_dest_near(0);
        self.host.map_search("");
        self.pump_dest(bridge);
        true
    }

    /// The typing screen on a screen of the list (a key typed there, or
    /// its top row).
    pub(super) fn dest_type(&mut self, ui: &AppWindow) {
        if !self.dest_busy {
            self.show_dest(ui, true);
        }
    }

    /// A key of the keyboard (or typed): appended, and searched again.
    pub(super) fn dest_key(&mut self, bridge: &Bridge<'_>, key: &str) {
        if self.dest_busy || self.dest_query.len() + key.len() > QUERY_MAX {
            return;
        }
        // No space to begin with, nor two in a row.
        if key == " " && (self.dest_query.is_empty() || self.dest_query.ends_with(' ')) {
            return;
        }
        self.dest_query.push_str(key);
        self.dest_search(bridge);
    }

    pub(super) fn dest_backspace(&mut self, bridge: &Bridge<'_>) {
        if self.dest_busy || self.dest_query.pop().is_none() {
            return;
        }
        self.dest_search(bridge);
    }

    fn dest_search(&mut self, bridge: &Bridge<'_>) {
        self.dest_error = false;
        self.host.map_search(&self.dest_query);
        self.show_query(bridge);
        self.pump_dest(bridge);
    }

    /// What is typed, the space after a word too (where the caret goes).
    fn show_query(&mut self, bridge: &Bridge<'_>) {
        let mut shown = clean(&mut self.scratch, &self.dest_query).to_string();
        if self.dest_query.ends_with(' ') {
            shown.push(' ');
        }
        bridge.set_dest_query(SharedString::from(shown));
    }

    /// The keyboard's OK: the list, when there is one to show.
    pub(super) fn dest_list(&mut self, ui: &AppWindow) {
        if !self.host.map_destinations().is_empty() {
            self.show_dest(ui, false);
        }
    }

    /// A destination tapped (or Enter on its row): the route to it.
    pub(super) fn dest_pick(&mut self, bridge: &Bridge<'_>, index: i32) {
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        if self.dest_busy || index >= self.host.map_destinations().len() {
            return;
        }
        // Choosing Home or Work: kept, said on the map's bar, no route.
        if let Some(slot) = self.dest_choose {
            let Some(place) = self.host.map_keep(index) else {
                return;
            };
            match Value::try_from(&place) {
                Ok(value) => self.persist(slot.key(), value),
                Err(e) => log::error!("{}: {e}", slot.key()),
            }
            let note = format!("{} set: {}", slot.name(), place.name);
            let until = self.now() + FLASH_MS;
            self.map_flash(&note, until);
            self.dest_choose = None;
            bridge.set_screen(MAP);
            return;
        }
        self.dest_busy = true;
        self.dest_error = false;
        self.host.map_go(index);
        self.pump_dest(bridge);
    }

    /// While a route is being found: whether it was, before the screen
    /// switch is looked at, so a route found goes back to the map in the
    /// same tick.
    pub(super) fn pump_dest_route(&mut self, bridge: &Bridge<'_>) {
        if !self.dest_busy {
            return;
        }
        let menu = std::mem::take(&mut self.menu_routing);
        match self.host.map_route_poll() {
            RouteStage::Finding => {
                self.menu_routing = menu;
                return;
            }
            RouteStage::Found => {
                if bridge.get_screen() == DEST {
                    bridge.set_screen(MAP);
                }
                if menu {
                    // The guidance says the rest.
                    self.map_flash("", 0);
                }
            }
            // From the menu, the bar says why; on Go to, its header.
            RouteStage::Failed if menu => {
                let note = self.host.map_route_note().to_string();
                let until = self.now() + FLASH_MS;
                self.map_flash(&note, until);
            }
            RouteStage::Failed => self.dest_error = true,
            RouteStage::Idle => {
                if menu {
                    self.map_flash("", 0);
                }
            }
        }
        self.dest_busy = false;
        self.pump_dest(bridge);
    }

    /// The rows, the header and the caption, when they changed.
    pub(super) fn pump_dest(&mut self, bridge: &Bridge<'_>) {
        if self.p_dests.as_deref() != Some(self.host.map_destinations()) {
            let found = self.host.map_destinations().to_vec();
            let rows: Vec<DestRow> = found
                .iter()
                .map(|d| DestRow {
                    name: clean(&mut self.scratch, &d.name),
                    detail: clean(&mut self.scratch, &d.detail),
                    place: d.place,
                })
                .collect();
            set_rows(&self.dest_top, &rows[..rows.len().min(TOP)]);
            set_rows(&self.dest_rows, &rows);
            // The ring's row may have gone with the list.
            let ring = bridge.get_ring_row();
            if ring.starts_with("dest-")
                && ring.as_str() != "dest-type"
                && ring[5..].parse::<usize>().is_ok_and(|i| i >= rows.len())
            {
                self.ring_gone(bridge);
            }
            self.p_dests = Some(found);
        }
        let none = self.host.map_destinations().is_empty();
        // The nearest of a kind: after its name ("Fuel · nearest first").
        let caption = match (self.dest_near, self.dest_query.is_empty(), none) {
            (Some(_), _, false) => "nearest first",
            (Some(_), _, true) => "none nearby",
            (None, true, false) => "Recent",
            (None, true, true) => "Type a street or a place",
            (None, false, true) => "No match",
            (None, false, false) => "",
        };
        if self.p_dest_caption.update(caption) {
            bridge.set_dest_caption(SharedString::from(caption));
        }
        let note = if self.dest_busy {
            "Finding the route"
        } else if self.dest_error {
            self.host.map_route_note()
        } else if self.dest_typing {
            match self.dest_choose {
                Some(MapSlot::Home) => "Set Home",
                Some(MapSlot::Work) => "Set Work",
                None => "Go to",
            }
        } else {
            ""
        };
        if self.p_dest_note.update(note) {
            let note = self.p_dest_note.as_str();
            bridge.set_dest_note(clean(&mut self.scratch, note));
        }
        bridge.set_dest_error(self.dest_error);
        bridge.set_dest_busy(self.dest_busy);
    }
}

/// A model's rows as `rows`, changing only what differs.
fn set_rows(model: &slint::VecModel<DestRow>, rows: &[DestRow]) {
    for (i, row) in rows.iter().enumerate() {
        if i < model.row_count() {
            if model.row_data(i).as_ref() != Some(row) {
                model.set_row_data(i, row.clone());
            }
        } else {
            model.push(row.clone());
        }
    }
    while model.row_count() > rows.len() {
        model.remove(model.row_count() - 1);
    }
}
