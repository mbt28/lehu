// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The map screen (19): the map is on the video plane, drawn by the
//! program; here only the bar over it, the drag and the keys.
//!
//! A drag moves the map with the finger. The arrows move it a quarter of
//! the panel, the knob zooms, Back leaves. The bar's Me key, there when
//! there is a position, centres the map on it.

use slint::SharedString;

use super::{Bridge, Host, MAP, State};
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
    }
}
