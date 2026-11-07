//! What BlueZ tells us about a device, and what the screens make of it:
//! whether it is an input device, what kind, and a name they can show.

use std::fmt;

/// A Bluetooth address, the six bytes of `AA:BB:CC:DD:EE:FF`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Address(pub [u8; 6]);

impl Address {
    /// From `AA:BB:CC:DD:EE:FF` (any case) or from BlueZ's
    /// `dev_AA_BB_CC_DD_EE_FF` path tail.
    pub fn parse(s: &str) -> Option<Self> {
        let tail = s.rsplit('/').next().unwrap_or(s);
        let tail = tail.strip_prefix("dev_").unwrap_or(tail);
        let mut out = [0u8; 6];
        let mut n = 0;
        for part in tail.split([':', '_']) {
            if n == 6 || part.len() != 2 {
                return None;
            }
            out[n] = u8::from_str_radix(part, 16).ok()?;
            n += 1;
        }
        (n == 6).then_some(Address(out))
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0; 6]
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let a = self.0;
        write!(
            f,
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            a[0], a[1], a[2], a[3], a[4], a[5]
        )
    }
}

/// The names shown on the screens are at most this long.
pub const NAME_CHARS: usize = 28;

/// One device in BlueZ's object tree, as far as the manager cares (the
/// screens get a `Device` row made from it).
#[derive(Clone, Debug, Default)]
pub struct Known {
    pub address: String,
    pub alias: String,
    pub icon: String,
    pub class: Option<u32>,
    pub appearance: Option<u16>,
    pub rssi: Option<i16>,
    /// HID (0x1124) or HID over GATT (0x1812) among its UUIDs.
    pub hid_uuid: bool,
    /// Its service UUIDs, lower case.
    pub uuids: Vec<String>,
    pub paired: bool,
    pub bonded: bool,
    pub trusted: bool,
    pub connected: bool,
    /// `org.bluez.Input1` is on it: a probed classic HID device.
    pub has_input: bool,
    /// `org.bluez.Battery1` percentage, while connected.
    pub battery: Option<u8>,
    /// First sighting: the lists never re-sort.
    pub order: u32,
    /// The number of the last discovery that reported it.
    pub seen_scan: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Keyboard,
    Mouse,
    Gamepad,
    Remote,
    Input,
}

impl Kind {
    pub fn text(self) -> &'static str {
        match self {
            Kind::Keyboard => "Keyboard",
            Kind::Mouse => "Mouse",
            Kind::Gamepad => "Gamepad",
            Kind::Remote => "Remote",
            Kind::Input => "Input device",
        }
    }
}

pub const UUID_HID: &str = "00001124-0000-1000-8000-00805f9b34fb";
pub const UUID_HOG: &str = "00001812-0000-1000-8000-00805f9b34fb";

/// Whether a UUID is HID or HID over GATT (BlueZ writes them lower-case,
/// the compare is case-blind anyway).
pub fn is_hid_uuid(uuid: &str) -> bool {
    uuid.eq_ignore_ascii_case(UUID_HID) || uuid.eq_ignore_ascii_case(UUID_HOG)
}

impl Known {
    fn major_class(&self) -> Option<u32> {
        self.class.map(|c| (c >> 8) & 0x1f)
    }

    /// An input device: Class of Device Peripheral, an `input-*` icon, a
    /// HID appearance, a HID UUID or the Input1 interface. A phone or a
    /// computer that also offers a HID role stays a phone.
    pub fn is_hid(&self) -> bool {
        match self.major_class() {
            Some(0x05) => return true,
            Some(0x01) | Some(0x02) => return false,
            _ => {}
        }
        if self.icon.starts_with("input-") {
            return true;
        }
        if let Some(a) = self.appearance
            && ((0x03c0..=0x03ff).contains(&a) || (0x0180..=0x01bf).contains(&a))
        {
            return true;
        }
        self.hid_uuid || self.has_input
    }

    pub fn kind(&self) -> Kind {
        if self.major_class() == Some(0x05)
            && let Some(c) = self.class
        {
            let minor = (c >> 2) & 0x3f;
            if minor & 0x10 != 0 {
                return Kind::Keyboard;
            }
            if minor & 0x20 != 0 {
                return Kind::Mouse;
            }
            match minor & 0x0f {
                1 | 2 => return Kind::Gamepad,
                3 => return Kind::Remote,
                _ => {}
            }
        }
        match self.icon.as_str() {
            "input-keyboard" => return Kind::Keyboard,
            "input-mouse" => return Kind::Mouse,
            "input-gaming" => return Kind::Gamepad,
            _ => {}
        }
        match self.appearance {
            Some(0x03c1) => Kind::Keyboard,
            Some(0x03c2) => Kind::Mouse,
            Some(0x03c3) | Some(0x03c4) => Kind::Gamepad,
            Some(a) if (0x0180..=0x01bf).contains(&a) => Kind::Remote,
            _ => Kind::Input,
        }
    }

    /// The alias (or the address) in printable ASCII only, white space
    /// collapsed, at most `NAME_CHARS` characters: the screens embed no
    /// other glyphs.
    pub fn display_name(&self) -> String {
        let raw = if self.alias.is_empty() {
            &self.address
        } else {
            &self.alias
        };
        let name = printable(raw, NAME_CHARS);
        if name.is_empty() {
            self.address.clone()
        } else {
            name
        }
    }
}

/// `s` reduced to printable ASCII with single spaces, at most `max` chars.
pub fn printable(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(max);
    let mut gap = false;
    for c in s.bytes() {
        if matches!(c, b' ' | b'\t' | b'\r' | b'\n') {
            gap = !out.is_empty();
            continue;
        }
        if !(0x20..=0x7e).contains(&c) {
            continue;
        }
        if gap {
            if out.len() + 1 >= max {
                break;
            }
            out.push(' ');
            gap = false;
        }
        if out.len() >= max {
            break;
        }
        out.push(c as char);
    }
    out
}

/// "123456" -> "1 2 3 4 5 6": easier to read off a car screen.
pub fn spaced(code: &str) -> String {
    let mut out = String::with_capacity(code.len() * 2);
    for c in code.chars() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

pub fn six_digits(value: u32) -> String {
    format!("{:06}", value % 1_000_000)
}

/// A random six-digit PIN for a legacy keyboard to type.
pub fn random_pin() -> String {
    let mut v: u32 = 0;
    // SAFETY: four bytes into a live u32.
    let n = unsafe { libc::getrandom((&mut v as *mut u32).cast(), 4, 0) };
    if n != 4 {
        v = (lehu_core::clock::now_ms() as u32).wrapping_mul(2_654_435_761);
    }
    six_digits(v)
}

/// The signal strength as the found list shows it.
pub fn signal_text(rssi: i16) -> &'static str {
    if rssi >= -60 {
        "strong"
    } else if rssi >= -75 {
        "good"
    } else {
        "weak"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse_from_strings_and_paths() {
        let a = Address::parse("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_0F").unwrap();
        assert_eq!(a.0, [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x0f]);
        assert_eq!(a.to_string(), "AA:BB:CC:DD:EE:0F");
        assert_eq!(Address::parse("aa:bb:cc:dd:ee:0f"), Some(a));
        assert_eq!(Address::parse("aa:bb:cc:dd:ee"), None);
        assert_eq!(Address::parse("aa:bb:cc:dd:ee:0f:00"), None);
        assert_eq!(Address::parse("/org/bluez/hci0"), None);
        assert!(Address::default().is_zero());
    }

    #[test]
    fn peripherals_are_hid_and_phones_are_not() {
        let mut d = Known {
            class: Some(0x002540),
            ..Default::default()
        };
        assert!(d.is_hid());
        assert_eq!(d.kind(), Kind::Keyboard);
        d.class = Some(0x002580);
        assert_eq!(d.kind(), Kind::Mouse);
        d.class = Some(0x002508);
        assert_eq!(d.kind(), Kind::Gamepad);
        d.class = Some(0x00250c);
        assert_eq!(d.kind(), Kind::Remote);
        // A phone that also lists the HID UUID stays a phone.
        let phone = Known {
            class: Some(0x5a020c),
            hid_uuid: true,
            ..Default::default()
        };
        assert!(!phone.is_hid());
        let le = Known {
            appearance: Some(0x03c1),
            ..Default::default()
        };
        assert!(le.is_hid());
        assert_eq!(le.kind(), Kind::Keyboard);
        let remote = Known {
            appearance: Some(0x0180),
            ..Default::default()
        };
        assert_eq!(remote.kind(), Kind::Remote);
        let icon = Known {
            icon: "input-gaming".into(),
            ..Default::default()
        };
        assert!(icon.is_hid());
        assert_eq!(icon.kind(), Kind::Gamepad);
        let probed = Known {
            has_input: true,
            ..Default::default()
        };
        assert!(probed.is_hid());
        assert_eq!(probed.kind(), Kind::Input);
        assert!(!Known::default().is_hid());
        assert!(is_hid_uuid("00001812-0000-1000-8000-00805F9B34FB"));
        assert!(!is_hid_uuid("0000111e-0000-1000-8000-00805f9b34fb"));
    }

    #[test]
    fn names_are_printable_ascii_and_short() {
        let d = Known {
            address: "AA:BB:CC:DD:EE:FF".into(),
            alias: "  K380\tKeyboard\n ünïcode  ".into(),
            ..Default::default()
        };
        assert_eq!(d.display_name(), "K380 Keyboard ncode");
        let long = Known {
            address: "AA:BB:CC:DD:EE:FF".into(),
            alias: "abcdefghijklmnopqrstuvwxyz0123456789".into(),
            ..Default::default()
        };
        assert_eq!(long.display_name().len(), NAME_CHARS);
        let blank = Known {
            address: "AA:BB:CC:DD:EE:FF".into(),
            alias: "\u{1f600}".into(),
            ..Default::default()
        };
        assert_eq!(blank.display_name(), "AA:BB:CC:DD:EE:FF");
        let none = Known {
            address: "AA:BB:CC:DD:EE:FF".into(),
            ..Default::default()
        };
        assert_eq!(none.display_name(), "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn codes_and_signal_texts() {
        assert_eq!(spaced("123456"), "1 2 3 4 5 6");
        assert_eq!(six_digits(42), "000042");
        assert_eq!(six_digits(1_234_567), "234567");
        assert_eq!(random_pin().len(), 6);
        assert_eq!(signal_text(-50), "strong");
        assert_eq!(signal_text(-70), "good");
        assert_eq!(signal_text(-80), "weak");
    }
}
