//! One keyboard event, on the loop thread.
//!
//! Dialogs take keys first. On the list screens Up/Down move Slint's focus
//! -- injected as Backtab/Tab, so the built-in traversal (conditional rows,
//! lists) and scroll-into-view do the work -- and Enter activates the
//! focused row. Back and Home never depend on focus: a recreated screen has
//! none. While a row picks in place (Settings, a phone on Devices) every key
//! is that row's; in the camera view they are the column's; in the editor
//! they go to the on-screen keyboard.

use slint::ComponentHandle;
use slint::platform::Key;

use super::{Bridge, CAMERA, HOME, Host, Pick, State, dispatch_key};
use crate::input::KeyEvent;
use crate::keys::OsdKey;
use crate::{AppWindow, Osk};

impl<H: Host> State<H> {
    pub(super) fn apply_key(&mut self, ui: &AppWindow, key: KeyEvent) {
        if key.value == 0 {
            return; // releases carry nothing: no modifier keys are sent to Slint
        }
        let repeat = key.value == 2;
        let nav = key.nav;
        self.last_key_ms = self.now();
        self.key_used = true;
        let bridge = ui.global::<Bridge<'_>>();
        // A held Backspace keeps deleting only if it was pressed in the
        // editor: the press that cancels a dialog over it must not then eat
        // the text beneath.
        if nav == OsdKey::Erase && !repeat {
            self.erase_edits = bridge.get_prompt_kind() == 0 && self.editing >= 0;
        }

        if matches!(nav, OsdKey::Home | OsdKey::Menu) {
            if repeat {
                return;
            }
            // Menu on the home screen, nothing open over it: back to the
            // phone's waiting session, as its tile. Elsewhere, Home.
            if nav == OsdKey::Menu
                && bridge.get_screen() == HOME
                && bridge.get_prompt_kind() == 0
                && self.editing < 0
            {
                if self.host.session_backgrounded() {
                    self.host.request_resume();
                }
                return;
            }
            if bridge.get_prompt_kind() != 0 {
                bridge.invoke_prompt_cancel();
            }
            if self.editing >= 0 {
                self.set_editing(ui, -1);
            }
            bridge.set_screen(HOME);
            return;
        }

        let kind = bridge.get_prompt_kind();
        if kind != 0 {
            if repeat {
                return;
            }
            // Buttons left to right: Cancel [accept] (a code dialog has only
            // Cancel); focus 1 is Cancel, 0 the accept. The arrows show and
            // move a ring over them; Enter presses the ringed button, or
            // accepts when there is no ring. Back always cancels. Our own
            // questions are a reboot, a shut down, a phone to forget: there
            // the ring starts on Cancel, and a press with no ring only
            // shows it -- a double press must not take the head unit down.
            let first = if kind == 2 { 0 } else { 1 };
            let start = if self.dialog_local { 1 } else { first };
            let focus = bridge.get_prompt_focus();
            match nav {
                OsdKey::Left | OsdKey::Up => {
                    bridge.set_prompt_focus(if focus < 0 { start } else { 1 })
                }
                OsdKey::Right | OsdKey::Down => {
                    bridge.set_prompt_focus(if focus < 0 { start } else { first })
                }
                // A knob goes round.
                OsdKey::Next => bridge.set_prompt_focus(if focus < 0 {
                    start
                } else if focus >= 1 {
                    first
                } else {
                    focus + 1
                }),
                OsdKey::Prev => bridge.set_prompt_focus(if focus < 0 {
                    start
                } else if focus <= first {
                    1
                } else {
                    focus - 1
                }),
                OsdKey::Ok => {
                    if focus < 0 && self.dialog_local {
                        bridge.set_prompt_focus(1);
                    } else if focus == 1 {
                        bridge.invoke_prompt_cancel();
                    } else if kind == 2 {
                        bridge.invoke_prompt_accept();
                    }
                }
                OsdKey::Back | OsdKey::Erase => bridge.invoke_prompt_cancel(),
                _ => {}
            }
            return;
        }

        // A row picking in place: every key is its own.
        if self.pick != Pick::None && bridge.get_screen() == self.pick_screen() {
            self.pick_key(&bridge, nav, repeat);
            return;
        }
        // The camera view: its settings column.
        if bridge.get_screen() == CAMERA {
            self.camera_key(&bridge, nav, repeat);
            return;
        }

        if self.editing >= 0 {
            // Arrows move the on-screen keyboard's ring (how a remote types)
            // and Enter presses the ringed key. Typing on a keyboard hides
            // the ring again, so its Enter saves.
            let osk = ui.global::<Osk<'_>>();
            match nav {
                OsdKey::Up => return osk.invoke_move(0, -1),
                OsdKey::Down => return osk.invoke_move(0, 1),
                OsdKey::Left => return osk.invoke_move(-1, 0),
                OsdKey::Right => return osk.invoke_move(1, 0),
                // A knob walks every key in reading order.
                OsdKey::Next => return osk.invoke_step(1),
                OsdKey::Prev => return osk.invoke_step(-1),
                _ => {}
            }
            // Through the same callbacks as the on-screen keyboard, so the
            // length limits and validation stay in one place.
            if nav == OsdKey::Erase {
                osk.set_row(-1);
                if !repeat || self.erase_edits {
                    bridge.invoke_osk_backspace(); // held Backspace keeps deleting
                }
            } else if repeat {
                return;
            } else if nav == OsdKey::Ok && key.ch != Some(' ') {
                if osk.get_row() >= 0 {
                    osk.invoke_activate();
                } else {
                    bridge.invoke_osk_ok();
                }
            } else if nav == OsdKey::Back {
                bridge.invoke_osk_cancel();
            } else if let Some(c) = key.ch.filter(|c| (' '..='~').contains(c)) {
                osk.set_row(-1);
                let mut utf8 = [0u8; 4];
                bridge.invoke_osk_key(slint::SharedString::from(&*c.encode_utf8(&mut utf8)));
            }
            return;
        }

        match nav {
            // A knob: the same focus chain, wrapping at its ends.
            OsdKey::Down | OsdKey::Next => dispatch_key(ui, Key::Tab, repeat),
            OsdKey::Up | OsdKey::Prev => dispatch_key(ui, Key::Backtab, repeat),
            OsdKey::Ok => {
                if !repeat {
                    dispatch_key(ui, Key::Return, false);
                }
            }
            // Remotes have no Back key; Left is the convention. Back from
            // the home screen returns to a paused session, the same as its
            // phone's tile.
            OsdKey::Back | OsdKey::Erase | OsdKey::Left => {
                if repeat || self.back(&bridge) {
                    return;
                }
                if self.host.session_backgrounded() {
                    self.host.request_resume();
                }
            }
            // Right elsewhere, and typing outside the editor: nothing to act on.
            _ => {}
        }
    }
}
