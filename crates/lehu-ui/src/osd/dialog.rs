//! The dialog shows Bluetooth's prompt -- pairing has a deadline, so it
//! wins -- or else our own question: a reboot or a shut down from the Power
//! row, the camera key waiting to be learned, a phone about to be
//! forgotten, an update to install; or a notice: how the last update went.

use toml::Value;

use super::{Bridge, Host, State, clean};
use crate::host::{Power, PromptKind};

/// Our own question, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ask {
    None,
    Reboot,
    Shutdown,
    /// Video > Camera key: press the key to learn it.
    CameraKey,
    /// Devices > a phone > Forget: the one `device_id` names.
    ForgetDevice,
    /// Update > Install: the head unit restarts.
    Install,
    /// How the last update went, once after the restart.
    UpdateOutcome,
}

/// A dialog's text; kind 0 none, 1 a code (Cancel only), 2 yes/no, 3 a
/// notice (OK only).
#[derive(Clone, Debug, Default)]
pub(super) struct DialogText {
    pub kind: i32,
    pub title: String,
    pub code: String,
    pub hint: String,
    pub accept: String,
}

pub(super) fn prompt_kind_id(kind: PromptKind) -> i32 {
    match kind {
        PromptKind::None => 0,
        PromptKind::Code => 1,
        PromptKind::Question => 2,
    }
}

const HINT_CAMERA_KEY: &str = "Press the key on the Bluetooth keyboard that should switch to \
                               the camera and back. Esc cancels.";
const HINT_REBOOT: &str = "LEHU closes, then the head unit starts again.";
const HINT_SHUTDOWN: &str = "LEHU closes and the head unit stops. It starts again when its \
                             power is switched off and on.";
const HINT_FORGET: &str = "The head unit unpairs the phone on every link and drops what it \
                           knows about it. To use it again, pair it again from the phone.";

impl<H: Host> State<H> {
    /// Our question was answered (or dropped).
    pub(super) fn answer_system(&mut self, yes: bool) {
        let asked = self.system_ask;
        self.system_ask = Ask::None;
        self.dialog_dirty = true;
        match asked {
            Ask::CameraKey => {
                self.host.set_key_learning(false);
                if yes {
                    // Clear
                    self.persist("camera.key", Value::Integer(0));
                }
            }
            Ask::Reboot if yes => self.host.request_power(Power::Reboot),
            Ask::Shutdown if yes => self.host.request_power(Power::Shutdown),
            Ask::ForgetDevice if yes => self.host.device_forget(&self.device_id),
            Ask::Install if yes => self.host.update_install(),
            Ask::UpdateOutcome => {
                // Interrupted: its accept opens the page to install again.
                if yes && self.host.update().outcome == crate::host::UpdateOutcome::Interrupted {
                    self.update_goto = true;
                }
                self.host.update_outcome_seen();
            }
            _ => {}
        }
    }

    /// Bluetooth's prompt, or else our own question -> Bridge, on change.
    pub(super) fn pump_dialog(&mut self, bridge: &Bridge<'_>) {
        if !self.dialog_dirty {
            return;
        }
        self.dialog_dirty = false;
        self.dialog_local = self.bt_dialog.kind == 0 && self.system_ask != Ask::None;
        let outcome = if self.dialog_local && self.system_ask == Ask::UpdateOutcome {
            self.outcome_dialog()
        } else {
            (0, "", "")
        };
        let bt = &self.bt_dialog;
        let restart = self.host.update().restart;
        let (kind, title, code, hint, accept): (i32, &str, &str, &str, &str) = if self.dialog_local
        {
            match self.system_ask {
                Ask::Install => (
                    2,
                    "Install the update?",
                    "",
                    if restart {
                        super::update::HINT_INSTALL_BOOT
                    } else {
                        super::update::HINT_INSTALL
                    },
                    "Install",
                ),
                Ask::UpdateOutcome => (outcome.0, outcome.1, "", &self.update_hint, outcome.2),
                Ask::CameraKey => (2, "Camera key", "", HINT_CAMERA_KEY, "Clear"),
                Ask::Reboot => (2, "Reboot the head unit?", "", HINT_REBOOT, "Reboot"),
                Ask::Shutdown => (
                    2,
                    "Shut down the head unit?",
                    "",
                    HINT_SHUTDOWN,
                    "Shut down",
                ),
                Ask::ForgetDevice => (2, self.device_ask.as_str(), "", HINT_FORGET, "Forget"),
                Ask::None => (0, "", "", "", ""),
            }
        } else {
            (bt.kind, &bt.title, &bt.code, &bt.hint, &bt.accept)
        };
        // Another dialog (or none). Our own questions take the head unit
        // down or drop something: for someone on the keys the ring starts
        // on Cancel. Bluetooth's start without one, so Enter accepts.
        if kind != self.p_prompt_kind || title != self.p_prompt_title.as_str() {
            let cancel = self.dialog_local && kind != 0 && self.keys_recent();
            bridge.set_prompt_focus(if cancel { 1 } else { -1 });
            let danger = self.dialog_local
                && matches!(
                    self.system_ask,
                    Ask::Reboot | Ask::Shutdown | Ask::ForgetDevice | Ask::Install
                );
            bridge.set_prompt_danger(danger);
        }
        if self.p_prompt_title.update(title) {
            bridge.set_prompt_title(clean(&mut self.scratch, title));
        }
        if self.p_prompt_code.update(code) {
            bridge.set_prompt_code(clean(&mut self.scratch, code));
        }
        if self.p_prompt_hint.update(hint) {
            bridge.set_prompt_hint(clean(&mut self.scratch, hint));
        }
        if self.p_prompt_accept.update(accept) {
            bridge.set_prompt_accept_label(clean(&mut self.scratch, accept));
        }
        // Kind last: the dialog appears with its text already in place.
        if kind != self.p_prompt_kind {
            if self.p_prompt_kind == 0 {
                self.remember_ring(bridge);
            }
            self.p_prompt_kind = kind;
            bridge.set_prompt_kind(kind);
        }
    }
}
