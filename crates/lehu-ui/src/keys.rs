// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Navigation keys the screens understand, independent of where they come
//! from (a Bluetooth keyboard or remote today). Kept free of evdev and Slint
//! so the key reader and the screens can share it.

use std::fmt::Write as _;

/// What a key means to the screens. The typed character travels beside it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OsdKey {
    /// No navigation meaning: only the typed character, if any, matters.
    #[default]
    None,
    Up,
    Down,
    Left,
    Right,
    /// Activate the focused row, confirm.
    Ok,
    /// Leave the current screen, dialog or editor.
    Back,
    /// Delete a character while editing; Back otherwise.
    Erase,
    /// Straight to the home screen.
    Home,
    /// The home screen; on it, back to the phone's waiting session.
    Menu,
    /// One rotary detent forward: the next focusable thing, wrapping.
    Next,
    /// One rotary detent back.
    Prev,
}

/// Linux key codes the Video screen has a name for (input-event-codes.h).
const NAMES: &[(u32, &str)] = &[
    (1, "Esc"),
    (15, "Tab"),
    (57, "Space"),
    (28, "Enter"),
    (14, "Backspace"),
    (110, "Insert"),
    (111, "Delete"),
    (102, "Home"),
    (107, "End"),
    (104, "Page Up"),
    (109, "Page Down"),
    (103, "Up"),
    (108, "Down"),
    (105, "Left"),
    (106, "Right"),
    (99, "Print"),
    (70, "Scroll Lock"),
    (119, "Pause"),
    (58, "Caps Lock"),
    (69, "Num Lock"),
    (127, "Menu"),
    (172, "Home page"),
    (217, "Search"),
    (158, "Back"),
    (164, "Play/Pause"),
    (163, "Next"),
    (165, "Previous"),
    (166, "Stop"),
    (155, "Mail"),
    (140, "Calculator"),
    (212, "Camera"),
    (139, "Menu"),
    (125, "Win"),
    (126, "Win"),
    (29, "Ctrl"),
    (97, "Ctrl"),
    (56, "Alt"),
    (100, "Alt Gr"),
    (42, "Shift"),
    (54, "Shift"),
];

const KEY_1: u32 = 2;
const KEY_0: u32 = 11;
const KEY_Q: u32 = 16;
const KEY_A: u32 = 30;
const KEY_Z: u32 = 44;
const KEY_F1: u32 = 59;
const KEY_F10: u32 = 68;
const KEY_F11: u32 = 87;
const KEY_F12: u32 = 88;

/// A key as the Video screen names it (the camera key row), from its Linux
/// key code; 0 is "None". Written into `out`, so the pump allocates nothing.
pub fn key_name(out: &mut String, code: u32) {
    out.clear();
    if code == 0 {
        out.push_str("None");
        return;
    }
    if (KEY_F1..=KEY_F10).contains(&code) {
        let _ = write!(out, "F{}", code - KEY_F1 + 1);
        return;
    }
    if code == KEY_F11 || code == KEY_F12 {
        out.push_str(if code == KEY_F11 { "F11" } else { "F12" });
        return;
    }
    if let Some((_, name)) = NAMES.iter().find(|(c, _)| *c == code) {
        out.push_str(name);
        return;
    }
    const ROW1: &[u8] = b"QWERTYUIOP";
    const ROW2: &[u8] = b"ASDFGHJKL";
    const ROW3: &[u8] = b"ZXCVBNM";
    let letter = |row: &[u8], first: u32| row.get((code - first) as usize).map(|c| char::from(*c));
    let letter = if (KEY_Q..KEY_Q + 10).contains(&code) {
        letter(ROW1, KEY_Q)
    } else if (KEY_A..KEY_A + 9).contains(&code) {
        letter(ROW2, KEY_A)
    } else if (KEY_Z..KEY_Z + 7).contains(&code) {
        letter(ROW3, KEY_Z)
    } else {
        None
    };
    if let Some(c) = letter {
        out.push(c);
        return;
    }
    if (KEY_1..KEY_0).contains(&code) {
        let _ = write!(out, "{}", code - KEY_1 + 1);
        return;
    }
    if code == KEY_0 {
        out.push('0');
        return;
    }
    let _ = write!(out, "Key {code}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(code: u32) -> String {
        let mut s = String::new();
        key_name(&mut s, code);
        s
    }

    #[test]
    fn names_follow_the_old_screen() {
        assert_eq!(name(0), "None");
        assert_eq!(name(1), "Esc");
        assert_eq!(name(59), "F1");
        assert_eq!(name(68), "F10");
        assert_eq!(name(87), "F11");
        assert_eq!(name(88), "F12");
        assert_eq!(name(16), "Q");
        assert_eq!(name(25), "P");
        assert_eq!(name(30), "A");
        assert_eq!(name(38), "L");
        assert_eq!(name(44), "Z");
        assert_eq!(name(50), "M");
        assert_eq!(name(2), "1");
        assert_eq!(name(10), "9");
        assert_eq!(name(11), "0");
        assert_eq!(name(212), "Camera");
        assert_eq!(name(999), "Key 999");
    }
}
