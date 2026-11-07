//! The camera view (screen 9): the settings column over the picture is up
//! for a few seconds after the last input. Two pages: the menu (Back, the
//! inputs, Standard, Picture) and Picture (Back, the levels). The page,
//! ring and editing live here; the values in the camera, behind the host.

use slint::{Model, SharedString};

use super::{Bridge, CAMERA, CAMERA_PANEL_MS, Host, State, compose};
use crate::CameraRow;
use crate::keys::OsdKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CamItem {
    Back,
    Input,
    Standard,
    Picture,
    Level,
}

/// What a row of the page is: the item, and which input or level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CamRef {
    pub item: CamItem,
    pub index: usize,
}

impl<H: Host> State<H> {
    pub(super) fn show_panel(&mut self) {
        self.panel_up = true;
        self.panel_until = self.now() + CAMERA_PANEL_MS;
    }

    pub(super) fn hide_panel(&mut self) {
        self.panel_up = false;
        self.cam_editing = false;
    }

    /// The ringed row, if it is a level: which one.
    pub(super) fn ringed_level(&self) -> Option<usize> {
        let row = usize::try_from(self.cam_row).ok()?;
        match self.cam_items.get(row)? {
            CamRef {
                item: CamItem::Level,
                index,
            } => Some(*index),
            _ => None,
        }
    }

    /// The page's rows from the camera's state, synced into the model in
    /// place (a row instance survives, so the column does not flicker).
    fn build_camera_rows(&mut self) {
        let page = self.cam_page;
        let Self {
            host,
            cam_items,
            scratch,
            ..
        } = self;
        cam_items.clear();
        let mut rows: Vec<CameraRow> = Vec::with_capacity(12);
        let mut add = |item: CamItem, index: usize, label: &str, value: &str, active: bool| {
            cam_items.push(CamRef { item, index });
            rows.push(CameraRow {
                label: SharedString::from(label),
                value: SharedString::from(value),
                active,
            });
        };
        add(CamItem::Back, 0, "Back", "", false);
        if page == 0 {
            let shown = host.camera_input();
            for i in 0..host.camera_inputs() {
                let on = i == shown;
                let value = if on && host.camera_no_signal() {
                    "No signal"
                } else {
                    ""
                };
                let label = compose(scratch, format_args!("Input {}", i + 1));
                add(CamItem::Input, i, label, value, on);
            }
            if host.camera_has_standard() {
                add(
                    CamItem::Standard,
                    0,
                    "Standard",
                    host.camera_standard_label(),
                    false,
                );
            }
            if !host.camera_levels().is_empty() {
                add(CamItem::Picture, 0, "Picture", ">", false);
            }
        } else {
            for (i, level) in host.camera_levels().iter().enumerate() {
                add(CamItem::Level, i, &level.label, &level.value, false);
            }
        }

        let model = &self.cam_rows;
        while model.row_count() > rows.len() {
            model.remove(model.row_count() - 1);
        }
        for (i, row) in rows.into_iter().enumerate() {
            if i >= model.row_count() {
                model.push(row);
            } else if model.row_data(i).as_ref() != Some(&row) {
                model.set_row_data(i, row);
            }
        }
        let last = self.cam_items.len() as i32 - 1;
        self.cam_row = if self.cam_row < 0 {
            last
        } else {
            self.cam_row.min(last)
        };
    }

    /// Back from Picture to the menu (the ring on Picture), or out of the view.
    fn camera_back(&mut self, bridge: &Bridge<'_>) {
        self.cam_editing = false;
        if self.cam_page == 1 {
            self.cam_page = 0;
            self.cam_row = -1;
            self.cam_rows_dirty = true;
        } else {
            bridge.set_screen(self.cam_return);
        }
    }

    /// A press on a row -- Enter, a knob click (`key`) or a tap: Back goes
    /// back, an input becomes the one shown, Standard cycles, Picture opens
    /// its page, a level starts editing (a tap only rings it: - / + change
    /// it).
    pub(super) fn camera_press(&mut self, bridge: &Bridge<'_>, row: i32, key: bool) {
        let Some(item) = usize::try_from(row)
            .ok()
            .and_then(|i| self.cam_items.get(i))
            .copied()
        else {
            return;
        };
        match item.item {
            CamItem::Back => self.camera_back(bridge),
            CamItem::Input => self.host.camera_select_input(item.index),
            CamItem::Standard => self.host.camera_cycle_standard(1),
            CamItem::Picture => {
                self.cam_page = 1;
                self.cam_row = 1; // the first level
                self.cam_rows_dirty = true;
            }
            CamItem::Level => self.cam_editing = key,
        }
    }

    /// Turning the ringed row, when it is a level.
    fn turn(&mut self, item: CamRef, direction: i32) {
        if item.item == CamItem::Level {
            self.host.camera_adjust_level(item.index, direction);
        }
    }

    /// A key in the camera view. Turning never changes an input or the
    /// standard: a standard change restarts the stream.
    pub(super) fn camera_key(&mut self, bridge: &Bridge<'_>, nav: OsdKey, repeat: bool) {
        if !self.panel_up {
            // Hidden: only Back leaves; anything else brings the column back.
            if repeat {
                return;
            }
            if matches!(nav, OsdKey::Back | OsdKey::Erase) {
                bridge.set_screen(self.cam_return);
            } else {
                self.show_panel();
            }
            return;
        }
        self.show_panel();
        let count = self.cam_items.len() as i32;
        if count == 0 {
            return;
        }
        self.cam_row = self.cam_row.clamp(0, count - 1);
        let Some(item) = self.cam_items.get(self.cam_row as usize).copied() else {
            return;
        };
        match nav {
            OsdKey::Back | OsdKey::Erase => {
                if repeat {
                    return;
                }
                if self.cam_editing {
                    self.cam_editing = false;
                } else {
                    self.camera_back(bridge);
                }
            }
            OsdKey::Ok => {
                if repeat {
                    return;
                }
                if self.cam_editing {
                    self.cam_editing = false;
                } else {
                    self.camera_press(bridge, self.cam_row, true);
                }
            }
            OsdKey::Next | OsdKey::Down => {
                if self.cam_editing {
                    self.turn(item, if nav == OsdKey::Next { 1 } else { -1 });
                } else {
                    self.cam_row = (self.cam_row + 1) % count;
                }
            }
            OsdKey::Prev | OsdKey::Up => {
                if self.cam_editing {
                    self.turn(item, if nav == OsdKey::Prev { -1 } else { 1 });
                } else {
                    self.cam_row = (self.cam_row + count - 1) % count;
                }
            }
            OsdKey::Left | OsdKey::Right => {
                let direction = if nav == OsdKey::Right { 1 } else { -1 };
                if item.item == CamItem::Standard {
                    self.host.camera_cycle_standard(direction);
                } else {
                    self.turn(item, direction);
                }
            }
            _ => {}
        }
    }

    /// Camera -> the column: rows on the camera's generation or a page change
    /// (one load per tick otherwise), the page, ring and editing on change,
    /// the timed hide.
    pub(super) fn pump_camera(&mut self, bridge: &Bridge<'_>, screen: i32) {
        let generation = self.host.camera_generation();
        if !self.cam_seen || generation != self.p_cam_gen || self.cam_rows_dirty {
            self.cam_seen = true;
            self.p_cam_gen = generation;
            self.cam_rows_dirty = false;
            self.build_camera_rows();
        }
        if self.panel_up && (screen != CAMERA || self.now() >= self.panel_until) {
            self.hide_panel();
        }
        if self.cam_page != self.p_cam_page {
            self.p_cam_page = self.cam_page;
            bridge.set_camera_page(self.cam_page);
        }
        if self.cam_row != self.p_cam_row {
            self.p_cam_row = self.cam_row;
            bridge.set_camera_row(self.cam_row);
        }
        if self.cam_editing != self.p_cam_editing {
            self.p_cam_editing = self.cam_editing;
            bridge.set_camera_editing(self.cam_editing);
        }
    }
}
