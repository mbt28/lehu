// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The map screen (19): the map is on the video plane, drawn by the
//! program; here only the bar over it, its menu, the drag and the keys.
//!
//! The bar's keys never move: - + menu Back at its right end, Me just left
//! of them while the map is off where we are, and a muted key left of that
//! while the voice is off. A drag moves the map with the finger and the
//! arrows a quarter of the panel; the knob zooms, its click opens the menu,
//! Back leaves.
//!
//! The menu is a column of rows at the right, over the map: My position
//! (while the map is off it), Stop route (on a route of ours), Go to, Home,
//! Work (with the road data) and Voice. The knob walks it as any list --
//! the ring on its first row -- its click chooses, Back closes it; a tap
//! chooses, a tap on the map beside it or on the menu key closes it, and
//! so does leaving it alone a few seconds. Home and Work route there at
//! once; one not set yet opens Go to to choose it (see `dest.rs`).

use lehu_core::settings::MapPlace;
use slint::platform::Key;
use slint::{ComponentHandle, SharedString};
use toml::Value;

use super::{Bridge, DEST, Host, MAP, State, clean, dispatch_key};
use crate::AppWindow;
use crate::host::MapSlot;
use crate::keys::OsdKey;

/// An arrow moves the map this far: a quarter of the 800 x 480 panel.
const STEP_X: i32 = 200;
const STEP_Y: i32 = 120;
/// The menu closes this long after the last input.
pub(super) const MENU_MS: u64 = 8000;
/// A note in the bar ("Home set") stays this long.
pub(super) const FLASH_MS: u64 = 4000;

/// The menu's rows, as `Bridge.map-menu-pick` numbers them.
const PICK_POSITION: i32 = 0;
const PICK_STOP: i32 = 1;
const PICK_GOTO: i32 = 2;
const PICK_HOME: i32 = 3;
const PICK_WORK: i32 = 4;
const PICK_VOICE: i32 = 5;

impl<H: Host> State<H> {
    /// A finger went down on the map: drags count from here.
    pub(super) fn map_drag_start(&mut self) {
        self.map_drag = (0, 0);
    }

    /// The finger is (`dx`, `dy`) from where it went down: the map follows
    /// by what it moved since the last report.
    pub(super) fn map_drag(&mut self, dx: i32, dy: i32) {
        let (mx, my) = (dx - self.map_drag.0, dy - self.map_drag.1);
        self.map_drag = (dx, dy);
        if mx != 0 || my != 0 {
            // The map moves with the finger: the view the other way.
            self.host.map_pan(-mx, -my);
        }
    }

    pub(super) fn map_key(&mut self, ui: &AppWindow, nav: OsdKey, repeat: bool) {
        let bridge = ui.global::<Bridge<'_>>();
        if self.map_menu {
            // The menu's rows, as any list: Tab and Backtab move the ring.
            self.menu_until = self.now() + MENU_MS;
            match nav {
                OsdKey::Next | OsdKey::Down => dispatch_key(ui, Key::Tab, repeat),
                OsdKey::Prev | OsdKey::Up => dispatch_key(ui, Key::Backtab, repeat),
                // A click with no ring (the menu opened by touch) shows it.
                OsdKey::Ok if !repeat && bridge.get_ring_row().is_empty() => {
                    dispatch_key(ui, Key::Tab, false);
                }
                OsdKey::Ok if !repeat => dispatch_key(ui, Key::Return, false),
                OsdKey::Back | OsdKey::Erase | OsdKey::Left if !repeat => {
                    self.map_menu_show(&bridge, false);
                }
                _ => {}
            }
            return;
        }
        match nav {
            OsdKey::Up => self.host.map_pan(0, -STEP_Y),
            OsdKey::Down => self.host.map_pan(0, STEP_Y),
            OsdKey::Left => self.host.map_pan(-STEP_X, 0),
            OsdKey::Right => self.host.map_pan(STEP_X, 0),
            // The knob zooms: clockwise in; its click opens the menu.
            OsdKey::Next => self.host.map_zoom(1),
            OsdKey::Prev => self.host.map_zoom(-1),
            OsdKey::Ok if !repeat => self.map_menu_show(&bridge, true),
            OsdKey::Back | OsdKey::Erase if !repeat => {
                self.back(&bridge);
            }
            _ => {}
        }
    }

    /// Open or close the menu. Opened, its Home and Work say where they
    /// are from here, and the ring (for someone on the knob) starts on
    /// its first row.
    pub(super) fn map_menu_show(&mut self, bridge: &Bridge<'_>, open: bool) {
        if open == self.map_menu {
            return;
        }
        self.map_menu = open;
        bridge.set_map_menu_open(open);
        // The rows go with it; nothing reports losing the ring.
        bridge.set_ring_row(SharedString::default());
        if open {
            self.menu_until = self.now() + MENU_MS;
            self.show_slots(bridge);
            self.focus_seed = true;
            self.ring_target = None;
        }
    }

    /// Home's and Work's second line: the place and how far it is.
    fn show_slots(&mut self, bridge: &Bridge<'_>) {
        for slot in [MapSlot::Home, MapSlot::Work] {
            let line = match slot.place(&self.host.settings().map) {
                Some(place) => {
                    let far = self.host.map_place_distance(place);
                    if far.is_empty() {
                        place.name.clone()
                    } else {
                        format!("{}, {far}", place.name)
                    }
                }
                None => String::new(),
            };
            let line = clean(&mut self.scratch, &line);
            match slot {
                MapSlot::Home => bridge.set_map_home(line),
                MapSlot::Work => bridge.set_map_work(line),
            }
        }
    }

    /// A row of the menu chosen, by the knob's click or a tap.
    pub(super) fn map_menu_pick(&mut self, bridge: &Bridge<'_>, row: i32) {
        if !self.map_menu {
            return;
        }
        if row == PICK_VOICE {
            // Stays open: the row says what it is now.
            let on = self.host.settings().map.voice;
            self.persist("map.voice", Value::Boolean(!on));
            self.menu_until = self.now() + MENU_MS;
            return;
        }
        self.map_menu_show(bridge, false);
        match row {
            PICK_POSITION => self.host.map_locate(),
            PICK_STOP => self.host.map_end_route(),
            PICK_GOTO => {
                self.dest_choose = None;
                bridge.set_screen(DEST);
            }
            PICK_HOME | PICK_WORK => {
                let slot = if row == PICK_HOME {
                    MapSlot::Home
                } else {
                    MapSlot::Work
                };
                match slot.place(&self.host.settings().map).cloned() {
                    Some(place) => self.map_go_place(slot, &place),
                    // Not set: Go to, to choose it.
                    None => {
                        self.dest_choose = Some(slot);
                        bridge.set_screen(DEST);
                    }
                }
            }
            _ => {}
        }
    }

    /// The route to a kept place, found as Go to finds one; the bar says
    /// so meanwhile, and why there is none if there is none.
    fn map_go_place(&mut self, slot: MapSlot, place: &MapPlace) {
        if self.dest_busy {
            return;
        }
        self.dest_busy = true;
        self.menu_routing = true;
        self.dest_error = false;
        self.host.map_go_place(place);
        // Up until the answer.
        let note = format!("Finding the route to {}", slot.name());
        self.map_flash(&note, u64::MAX);
    }

    /// A short note in the place of the bar's words, until `until`.
    pub(super) fn map_flash(&mut self, text: &str, until: u64) {
        self.flash.clear();
        self.flash.push_str(text);
        self.flash_until = until;
    }

    /// The bar's muted key: the voice on again.
    pub(super) fn map_voice(&mut self, on: bool) {
        if self.host.settings().map.voice != on {
            self.persist("map.voice", Value::Boolean(on));
        }
    }

    /// Whether Home and Work are set: the menu's rows and Go to's Change
    /// rows. Two option checks a tick.
    pub(super) fn pump_slots(&mut self, bridge: &Bridge<'_>) {
        let map = &self.host.settings().map;
        let set = (map.home.is_some(), map.work.is_some());
        if self.p_map_slots != Some(set) {
            self.p_map_slots = Some(set);
            bridge.set_map_home_set(set.0);
            bridge.set_map_work_set(set.1);
        }
    }

    /// The note over the map, the bar's words and keys, and the menu,
    /// when they changed.
    pub(super) fn pump_map(&mut self, bridge: &Bridge<'_>, screen: i32, now: u64) {
        if screen != MAP {
            // Gone with its screen.
            if self.map_menu {
                self.map_menu_show(bridge, false);
            }
            return;
        }
        if self.map_menu && now >= self.menu_until {
            self.map_menu_show(bridge, false);
        }
        if self.flash_until != 0 && now >= self.flash_until {
            self.flash_until = 0;
            self.flash.clear();
        }
        if self.p_map_flash.update(&self.flash) {
            let flash = self.p_map_flash.as_str();
            bridge.set_map_flash(clean(&mut self.scratch, flash));
        }
        if self.p_map_note.update(self.host.map_note()) {
            bridge.set_map_note(SharedString::from(self.p_map_note.as_str()));
        }
        if self.p_map_label.update(self.host.map_label()) {
            bridge.set_map_label(SharedString::from(self.p_map_label.as_str()));
        }
        let away = self.host.map_away();
        if self.p_map_away != Some(away) {
            self.p_map_away = Some(away);
            bridge.set_map_away(away);
            // My position may have had the ring.
            if !away && bridge.get_ring_row().as_str() == "menu-me" {
                self.ring_gone(bridge);
            }
        }
        let voice = self.host.settings().map.voice;
        if self.p_map_voice != Some(voice) {
            self.p_map_voice = Some(voice);
            bridge.set_map_voice(voice);
        }
        let guidance = self.host.map_guidance();
        if self.p_map_guidance.as_ref() != guidance {
            self.p_map_guidance = guidance.cloned();
            let g = guidance.cloned().unwrap_or_default();
            bridge.set_map_guiding(guidance.is_some());
            bridge.set_map_arrow(g.arrow.id());
            bridge.set_map_then(g.then.id());
            bridge.set_map_maneuver(SharedString::from(g.maneuver.as_str()));
            bridge.set_map_distance(SharedString::from(g.distance.as_str()));
            // Names may have accents the font has not.
            bridge.set_map_road(clean(&mut self.scratch, &g.road));
            bridge.set_map_eta(SharedString::from(g.eta.as_str()));
            bridge.set_map_eta_note(SharedString::from(g.eta_note.as_str()));
        }
        let search = (self.host.map_can_search(), self.host.map_on_route());
        if self.p_map_search != Some(search) {
            if !search.1 && bridge.get_ring_row().as_str() == "menu-stop" {
                self.ring_gone(bridge);
            }
            self.p_map_search = Some(search);
            bridge.set_map_can_search(search.0);
            bridge.set_map_on_route(search.1);
        }
        let speed = self.host.map_speed();
        if self.p_map_speed != Some(speed) {
            self.p_map_speed = Some(speed);
            let (kmh, limit) = match speed {
                Some((v, l)) => (v.to_string(), l.map(|l| l.to_string()).unwrap_or_default()),
                None => (String::new(), String::new()),
            };
            bridge.set_map_speed(SharedString::from(kmh.as_str()));
            bridge.set_map_limit(SharedString::from(limit.as_str()));
            // Over the limit by more than a speedometer's error.
            let over = matches!(speed, Some((v, Some(l))) if v > u32::from(l) + 3);
            bridge.set_map_over(over);
        }
    }
}
