// SPDX-License-Identifier: GPL-3.0-or-later
//! What each evdev key means here: the navigation keys of the screens, the
//! phone's buttons, the volume keys, and the character a key types in the
//! US layout. Pure tables, used by `keys` and by the device probe.

use crate::evdev::*;
use crate::{Nav, PhoneButton};

/// Gamepad buttons and d-pads act as the keyboard keys they stand for; a
/// wheel's click is the rotary knob's press.
pub fn normalize(code: u16) -> u16 {
    match code {
        BTN_DPAD_UP => KEY_UP,
        BTN_DPAD_DOWN => KEY_DOWN,
        BTN_DPAD_LEFT => KEY_LEFT,
        BTN_DPAD_RIGHT => KEY_RIGHT,
        BTN_SOUTH | BTN_MIDDLE => KEY_ENTER,
        BTN_EAST => KEY_ESC,
        BTN_START | BTN_MODE => KEY_HOMEPAGE,
        _ => code,
    }
}

/// The phone's button for a key. Typing keys, letters, digits, Space and
/// Backspace, are never shortcuts: in a phone's search field they would
/// select or go back instead of typing.
pub fn phone_button(code: u16) -> Option<PhoneButton> {
    Some(match code {
        KEY_UP => PhoneButton::Up,
        KEY_DOWN => PhoneButton::Down,
        KEY_LEFT => PhoneButton::Left,
        KEY_RIGHT => PhoneButton::Right,
        KEY_ENTER | KEY_KPENTER | KEY_OK | KEY_SELECT => PhoneButton::Select,
        KEY_ESC | KEY_BACK => PhoneButton::Back,
        KEY_HOME | KEY_HOMEPAGE => PhoneButton::Home,
        KEY_COMPOSE | KEY_MENU => PhoneButton::Menu,
        KEY_PLAYPAUSE => PhoneButton::PlayPause,
        KEY_PLAY | KEY_PLAYCD => PhoneButton::Play,
        KEY_PAUSE | KEY_PAUSECD => PhoneButton::Pause,
        KEY_NEXTSONG => PhoneButton::NextTrack,
        KEY_PREVIOUSSONG => PhoneButton::PrevTrack,
        KEY_VOICECOMMAND | KEY_SEARCH | KEY_ASSISTANT => PhoneButton::Siri,
        _ => return None,
    })
}

/// Media and voice keys reach the phone even while a screen is up.
pub fn phone_only(code: u16) -> bool {
    matches!(
        code,
        KEY_PLAYPAUSE
            | KEY_PLAY
            | KEY_PLAYCD
            | KEY_PAUSE
            | KEY_PAUSECD
            | KEY_NEXTSONG
            | KEY_PREVIOUSSONG
            | KEY_VOICECOMMAND
            | KEY_SEARCH
            | KEY_ASSISTANT
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeKey {
    Up,
    Down,
    Mute,
}

/// The volume keys set the head unit's own volume, whatever is on screen.
pub fn volume_key(code: u16) -> Option<VolumeKey> {
    match code {
        KEY_VOLUMEUP => Some(VolumeKey::Up),
        KEY_VOLUMEDOWN => Some(VolumeKey::Down),
        KEY_MUTE => Some(VolumeKey::Mute),
        _ => None,
    }
}

/// The navigation meaning of a key on the screens.
pub fn nav_key(code: u16) -> Option<Nav> {
    Some(match code {
        KEY_UP => Nav::Up,
        KEY_DOWN => Nav::Down,
        KEY_LEFT => Nav::Left,
        KEY_RIGHT => Nav::Right,
        KEY_ENTER | KEY_KPENTER | KEY_OK | KEY_SELECT | KEY_SPACE => Nav::Ok,
        KEY_ESC | KEY_BACK => Nav::Back,
        KEY_BACKSPACE => Nav::Erase,
        KEY_HOME | KEY_HOMEPAGE => Nav::Home,
        KEY_COMPOSE | KEY_MENU => Nav::Menu,
        _ => return None,
    })
}

// US layout, indexed by evdev code KEY_ESC (1) to KEY_SPACE (57).
const US_PLAIN: &[u8; 58] =
    b"\0\x001234567890-=\0\0qwertyuiop[]\0\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
const US_SHIFT: &[u8; 58] =
    b"\0\x00!@#$%^&*()_+\0\0QWERTYUIOP{}\0\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";

/// The character a key types, after Shift and Caps Lock; Caps Lock only
/// turns letters.
pub fn typed_char(code: u16, shift: bool, caps: bool) -> Option<char> {
    let i = usize::from(code);
    let plain = *US_PLAIN.get(i)?;
    if plain == 0 {
        return None;
    }
    let c = if plain.is_ascii_lowercase() {
        if shift != caps {
            plain.to_ascii_uppercase()
        } else {
            plain
        }
    } else if shift {
        US_SHIFT.get(i).copied().unwrap_or(plain)
    } else {
        plain
    };
    Some(char::from(c))
}

/// Does this reader act on the key at all?
pub fn useful(code: u16) -> bool {
    let n = normalize(code);
    phone_button(n).is_some()
        || nav_key(n).is_some()
        || typed_char(code, false, false).is_some()
        || volume_key(code).is_some()
}

/// Does the device report at least one key this reader acts on?
pub fn useful_keys(bits: &[usize]) -> bool {
    (1..KEY_CNT as u16).any(|code| has(bits, code) && useful(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_characters_follow_the_us_layout() {
        assert_eq!(typed_char(30, false, false), Some('a'));
        assert_eq!(typed_char(30, true, false), Some('A'));
        assert_eq!(typed_char(30, false, true), Some('A'));
        assert_eq!(typed_char(30, true, true), Some('a')); // Shift undoes Caps Lock
        assert_eq!(typed_char(2, false, true), Some('1')); // Caps Lock leaves digits
        assert_eq!(typed_char(2, true, false), Some('!'));
        assert_eq!(typed_char(KEY_SPACE, true, false), Some(' '));
        assert_eq!(typed_char(40, true, false), Some('"'));
        assert_eq!(typed_char(43, false, false), Some('\\'));
        assert_eq!(typed_char(55, false, false), Some('*'));
        assert_eq!(typed_char(KEY_ESC, false, false), None);
        assert_eq!(typed_char(KEY_ENTER, false, false), None);
        assert_eq!(typed_char(KEY_BACKSPACE, false, false), None);
        assert_eq!(typed_char(KEY_LEFTSHIFT, true, false), None);
        assert_eq!(typed_char(KEY_UP, false, false), None); // past the table
        assert_eq!(typed_char(0x3ff, false, false), None);
    }

    #[test]
    fn navigation_and_phone_maps() {
        assert_eq!(nav_key(KEY_UP), Some(Nav::Up));
        assert_eq!(nav_key(KEY_SPACE), Some(Nav::Ok));
        assert_eq!(nav_key(KEY_BACKSPACE), Some(Nav::Erase));
        assert_eq!(nav_key(KEY_COMPOSE), Some(Nav::Menu));
        assert_eq!(nav_key(KEY_HOMEPAGE), Some(Nav::Home));
        assert_eq!(nav_key(30), None);
        assert_eq!(phone_button(KEY_OK), Some(PhoneButton::Select));
        assert_eq!(phone_button(KEY_BACK), Some(PhoneButton::Back));
        assert_eq!(phone_button(KEY_MENU), Some(PhoneButton::Menu));
        assert_eq!(phone_button(KEY_SEARCH), Some(PhoneButton::Siri));
        assert_eq!(phone_button(KEY_SPACE), None); // typing keys are never shortcuts
        assert_eq!(phone_button(KEY_BACKSPACE), None);
        assert_eq!(phone_button(30), None);
        assert!(phone_only(KEY_NEXTSONG));
        assert!(!phone_only(KEY_ENTER));
        assert_eq!(volume_key(KEY_MUTE), Some(VolumeKey::Mute));
    }

    #[test]
    fn gamepads_and_wheel_clicks_normalise() {
        assert_eq!(normalize(BTN_DPAD_LEFT), KEY_LEFT);
        assert_eq!(normalize(BTN_SOUTH), KEY_ENTER);
        assert_eq!(normalize(BTN_MIDDLE), KEY_ENTER);
        assert_eq!(normalize(BTN_EAST), KEY_ESC);
        assert_eq!(normalize(BTN_MODE), KEY_HOMEPAGE);
        assert_eq!(normalize(KEY_UP), KEY_UP);
    }

    #[test]
    fn useful_keys_in_a_bit_set() {
        let mut set = [0usize; KEY_WORDS];
        assert!(!useful_keys(&set));
        let bit = |code: u16| {
            (
                usize::from(code) / WORD,
                1usize << (usize::from(code) % WORD),
            )
        };
        const WORD: usize = usize::BITS as usize;
        let (w, b) = bit(KEY_LEFTSHIFT);
        set[w] |= b; // a modifier alone is not useful
        assert!(!useful_keys(&set));
        let (w, b) = bit(BTN_MIDDLE);
        set[w] |= b; // a wheel's click is
        assert!(useful_keys(&set));
    }
}
