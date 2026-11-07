//! The HID devices the head unit declares in `/info` and the reports it
//! sends for them through the event channel: a two-contact touchscreen, a
//! knob with four buttons and a wheel, the media keys and the telephony
//! keys. The uuids match the `hidDevices` entries of the info plist.

pub const TOUCH_UUID: &str = "2a2a2a2a";
pub const KNOB_UUID: &str = "2a2a2a2b";
pub const MEDIA_UUID: &str = "2a2a2a2c";
pub const TELEPHONY_UUID: &str = "2a2a2a2d";

/// Contacts the touch descriptor declares: two cover pinch and rotate.
pub const TOUCH_CONTACTS: usize = 2;

/// A touch report is six bytes per contact: index, touching, X, Y.
pub const TOUCH_REPORT_LEN: usize = 6 * TOUCH_CONTACTS;

/// The media keys, as the index into the consumer-control usage array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Media {
    None = 0,
    Play = 1,
    Pause = 2,
    PlayPause = 3,
    Next = 4,
    Prev = 5,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Contact {
    pub slot: u8,
    /// Pixels.
    pub x: u16,
    pub y: u16,
    pub down: bool,
}

/// The multitouch report. Every slot reports its fixed transducer index:
/// the phone tracks a finger by the slot it keeps appearing in; absent
/// slots are idle.
pub fn touch_report(contacts: &[Contact]) -> [u8; TOUCH_REPORT_LEN] {
    let mut r = [0u8; TOUCH_REPORT_LEN];
    for (i, slot) in r.as_chunks_mut::<6>().0.iter_mut().enumerate() {
        slot[0] = i as u8;
    }
    for c in contacts {
        let Some(slot) = r.as_chunks_mut::<6>().0.get_mut(usize::from(c.slot)) else {
            continue;
        };
        slot[0] = c.slot;
        slot[1] = u8::from(c.down);
        slot[2..4].copy_from_slice(&c.x.to_le_bytes());
        slot[4..6].copy_from_slice(&c.y.to_le_bytes());
    }
    r
}

pub fn media_report(key: Media) -> [u8; 1] {
    [key as u8]
}

/// The knob: buttons (select 1, home 2, back 4), then X and Y as a joystick
/// and the wheel, each a signed byte.
pub fn knob_report(select: bool, home: bool, back: bool, x: i8, y: i8, wheel: i8) -> [u8; 4] {
    [
        u8::from(select) | (u8::from(home) << 1) | (u8::from(back) << 2),
        x as u8,
        y as u8,
        wheel as u8,
    ]
}

/// The touchscreen descriptor: a digitizer application collection holding
/// `TOUCH_CONTACTS` finger collections, each with the contact index, the
/// tip switch and absolute X/Y up to the screen size.
pub fn touch_descriptor(width: u16, height: u16) -> Vec<u8> {
    let mut d = vec![0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01];
    let [wl, wh] = width.to_le_bytes();
    let [hl, hh] = height.to_le_bytes();
    for _ in 0..TOUCH_CONTACTS {
        d.extend_from_slice(&[
            0x05, 0x0d, 0x09, 0x22, 0xa1, 0x02, 0x09, 0x38, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02,
            0x15, 0x00, 0x25, 0x01, 0x09, 0x33, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x95, 0x07,
            0x81, 0x03, 0x05, 0x01, 0x26, wl, wh, 0x09, 0x30, 0x75, 0x10, 0x95, 0x01, 0x81, 0x02,
            0x26, hl, hh, 0x09, 0x31, 0x81, 0x02, 0xc0,
        ]);
    }
    d.push(0xc0);
    d
}

/// The knob: a generic-desktop multi-axis controller with select, two
/// consumer buttons (home, back), an X/Y pointer and a relative wheel.
pub const KNOB_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x08, 0xa1, 0x01, 0x05, 0x09, 0x09, 0x01, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01,
    0x95, 0x01, 0x81, 0x02, 0x05, 0x0c, 0x0a, 0x23, 0x02, 0x0a, 0x24, 0x02, 0x95, 0x02, 0x81, 0x02,
    0x95, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81,
    0x25, 0x7f, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02, 0xc0, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75,
    0x08, 0x95, 0x01, 0x81, 0x06, 0xc0,
];

/// Consumer control: one byte selecting play, pause, play/pause, next,
/// previous or the navigation-guidance usage.
pub const MEDIA_DESCRIPTOR: &[u8] = &[
    0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01, 0x15, 0x00, 0x25, 0x06, 0x05, 0x0c, 0x0a, 0x00, 0x00, 0x0a,
    0xb0, 0x00, 0x0a, 0xb1, 0x00, 0x0a, 0xcd, 0x00, 0x0a, 0xb5, 0x00, 0x0a, 0xb6, 0x00, 0x0a, 0x9e,
    0x02, 0x75, 0x08, 0x95, 0x01, 0x81, 0x00, 0xc0,
];

/// Telephony keys: one byte selecting a telephony usage.
pub const TELEPHONY_DESCRIPTOR: &[u8] = &[
    0x05, 0x0b, 0x09, 0x07, 0xa1, 0x01, 0x15, 0x00, 0x25, 0x11, 0x05, 0x0b, 0x09, 0x00, 0x09, 0x20,
    0x09, 0x21, 0x09, 0x26, 0x09, 0x2f, 0x09, 0xb0, 0x09, 0xb1, 0x09, 0xb2, 0x09, 0xb3, 0x09, 0xb4,
    0x09, 0xb5, 0x09, 0xb6, 0x09, 0xb7, 0x09, 0xb8, 0x09, 0xb9, 0x09, 0xba, 0x09, 0xbb, 0x05, 0x07,
    0x09, 0x2a, 0x75, 0x08, 0x95, 0x01, 0x81, 0x00, 0xc0,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_report_layout() {
        let r = touch_report(&[Contact {
            slot: 1,
            x: 0x1234,
            y: 0x0102,
            down: true,
        }]);
        assert_eq!(r, [0, 0, 0, 0, 0, 0, 1, 1, 0x34, 0x12, 0x02, 0x01]);
        let idle = touch_report(&[]);
        assert_eq!(idle, [0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0]);
        // A slot beyond the descriptor is ignored.
        let r = touch_report(&[Contact {
            slot: 5,
            x: 1,
            y: 1,
            down: true,
        }]);
        assert_eq!(r, idle);
    }

    #[test]
    fn knob_and_media() {
        assert_eq!(knob_report(true, false, true, -127, 0, 1), [5, 0x81, 0, 1]);
        assert_eq!(media_report(Media::Next), [4]);
        let d = touch_descriptor(800, 480);
        assert_eq!(d.len(), 6 + 2 * 51 + 1);
        assert_eq!(&d[6 + 32..6 + 35], &[0x26, 0x20, 0x03]);
    }
}
