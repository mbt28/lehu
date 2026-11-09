// SPDX-License-Identifier: GPL-3.0-or-later
//! The dialog the screens show for Bluetooth, and its texts.

use crate::device::{six_digits, spaced};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PromptKind {
    #[default]
    None,
    /// A code to type on the device; Cancel is the only button.
    Code,
    /// A yes/no question.
    Question,
}

/// The dialog as the manager has it; maps one-to-one onto the screens'.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prompt {
    pub kind: PromptKind,
    pub title: String,
    /// Pre-formatted ("1 2 3 4 5 6"); empty when there is none.
    pub code: String,
    pub hint: String,
    /// The accept button's label (a question only).
    pub accept: String,
}

impl Prompt {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn is_none(&self) -> bool {
        self.kind == PromptKind::None
    }

    /// A code the device must type, with the progress the device reports
    /// (0 when none).
    pub fn code(name: &str, code: &str, entered: u16) -> Self {
        let mut hint = String::from("Type the code on the keyboard, then press Enter");
        if entered > 0 {
            hint.push_str(&format!(" ({entered} of {} typed)", code.len()));
        }
        Self {
            kind: PromptKind::Code,
            title: format!("Pair {name}"),
            code: spaced(code),
            hint,
            accept: String::new(),
        }
    }

    /// A device with a display: the user compares the codes.
    pub fn confirm(name: &str, passkey: u32) -> Self {
        Self {
            kind: PromptKind::Question,
            title: format!("Pair {name}"),
            code: spaced(&six_digits(passkey)),
            hint: format!("Pair if {name} shows the same code."),
            accept: "Pair".into(),
        }
    }

    /// The user's own Forget, confirmed first.
    pub fn forget(name: &str) -> Self {
        Self {
            kind: PromptKind::Question,
            title: format!("Forget {name}?"),
            code: String::new(),
            hint: "To use it again, pair it again from Add device.".into(),
            accept: "Forget".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_texts() {
        let p = Prompt::code("K380", "123456", 0);
        assert_eq!(p.kind, PromptKind::Code);
        assert_eq!(p.title, "Pair K380");
        assert_eq!(p.code, "1 2 3 4 5 6");
        assert_eq!(p.hint, "Type the code on the keyboard, then press Enter");
        assert!(p.accept.is_empty());
        let p = Prompt::code("K380", "123456", 3);
        assert_eq!(
            p.hint,
            "Type the code on the keyboard, then press Enter (3 of 6 typed)"
        );
        let p = Prompt::confirm("Remote", 42);
        assert_eq!(p.kind, PromptKind::Question);
        assert_eq!(p.code, "0 0 0 0 4 2");
        assert_eq!(p.hint, "Pair if Remote shows the same code.");
        assert_eq!(p.accept, "Pair");
        let p = Prompt::forget("K380");
        assert_eq!(p.title, "Forget K380?");
        assert_eq!(p.hint, "To use it again, pair it again from Add device.");
        assert_eq!(p.accept, "Forget");
        assert!(p.code.is_empty());
        assert!(Prompt::none().is_none());
        assert!(!p.is_none());
    }
}
