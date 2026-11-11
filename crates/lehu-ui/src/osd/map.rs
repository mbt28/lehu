// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The map screen (19): the map is on the video plane, drawn by the
//! program; here only the bar over it, the drag and the keys.
//!
//! A drag moves the map with the finger. The arrows move it a quarter of
//! the panel, the knob zooms, Back leaves. The bar's Me key, there when
//! there is a position, centres the map on it; its Go to key, there when
//! the map has names to search, opens the destination screen (see
//! `dest.rs`), and Stop ends a route found there.

use slint::SharedString;

use super::{Bridge, Host, MAP, State, clean};
use crate::keys::OsdKey;

/// An arrow moves the map this far: a quarter of the 800 x 480 panel.
const STEP_X: i32 = 200;
const STEP_Y: i32 = 120;

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

    pub(super) fn map_key(&mut self, bridge: &Bridge<'_>, nav: OsdKey, repeat: bool) {
        match nav {
            OsdKey::Up => self.host.map_pan(0, -STEP_Y),
            OsdKey::Down => self.host.map_pan(0, STEP_Y),
            OsdKey::Left => self.host.map_pan(-STEP_X, 0),
            OsdKey::Right => self.host.map_pan(STEP_X, 0),
            // The knob zooms: clockwise in.
            OsdKey::Next => self.host.map_zoom(1),
            OsdKey::Prev => self.host.map_zoom(-1),
            OsdKey::Ok if !repeat => self.host.map_zoom(1),
            OsdKey::Back | OsdKey::Erase if !repeat => {
                self.back(bridge);
            }
            _ => {}
        }
    }

    /// The note over the map and the bar's words, when they changed.
    pub(super) fn pump_map(&mut self, bridge: &Bridge<'_>, screen: i32) {
        if screen != MAP {
            return;
        }
        if self.p_map_note.update(self.host.map_note()) {
            bridge.set_map_note(SharedString::from(self.p_map_note.as_str()));
        }
        if self.p_map_label.update(self.host.map_label()) {
            bridge.set_map_label(SharedString::from(self.p_map_label.as_str()));
        }
        let has = self.host.map_has_position();
        if self.p_map_position != Some(has) {
            self.p_map_position = Some(has);
            bridge.set_map_has_position(has);
        }
        let guidance = self.host.map_guidance();
        if self.p_map_guidance.as_ref() != guidance {
            self.p_map_guidance = guidance.cloned();
            let g = guidance.cloned().unwrap_or_default();
            bridge.set_map_guiding(guidance.is_some());
            bridge.set_map_arrow(g.arrow.id());
            bridge.set_map_maneuver(SharedString::from(g.maneuver.as_str()));
            bridge.set_map_distance(SharedString::from(g.distance.as_str()));
            // A road's name may have accents the font has not.
            bridge.set_map_road(clean(&mut self.scratch, &g.road));
            bridge.set_map_eta(SharedString::from(g.eta.as_str()));
            bridge.set_map_eta_note(SharedString::from(g.eta_note.as_str()));
        }
        let search = (self.host.map_can_search(), self.host.map_on_route());
        if self.p_map_search != Some(search) {
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
