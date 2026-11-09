// SPDX-License-Identifier: GPL-3.0-or-later
//! The Android Open Accessory switch: puts an attached phone into accessory
//! mode, so that it re-enumerates as 18d1:2d00 (or 2d01) with a pair of
//! bulk endpoints and starts its Android Auto stack.
//! <https://source.android.com/docs/core/interaction/accessories/aoa>
//!
//! The main loop calls `switch_to_accessory` for a device that appeared
//! and is neither a hub nor Apple's; the session is started on the
//! accessory that appears in its place.
//!
//! The switch itself (the protocol query, the strings, `vendor_out`)
//! follows LIVI's livi-aa/src/usb.rs (Lasse Heitgres, GPL-3.0); the
//! interface filter is FastCarPlay's aoap.cpp.

use std::fs;
use std::os::fd::OwnedFd;
use std::time::Duration;

use nusb::MaybeFuture;
use nusb::transfer::{ControlIn, ControlOut, ControlType, Recipient};

use crate::consts::*;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

/// What the switch found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Switch {
    /// The phone accepted the switch and is re-enumerating.
    Switched,
    /// Not a phone: a device with an interface class that is never a
    /// phone's, left alone. The name says which.
    Skipped(&'static str),
    /// The device does not speak the accessory protocol.
    NotSupported,
}

#[derive(Debug)]
pub enum Error {
    Open(std::io::Error),
    Usb(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Open(e) => write!(f, "opening the device: {e}"),
            Error::Usb(e) => f.write_str(e),
        }
    }
}

/// Open a device's usbfs node.
pub(crate) fn open_node(device: &lehu_usb::Device) -> Result<nusb::Device, Error> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device.node())
        .map_err(Error::Open)?;
    nusb::Device::from_fd(OwnedFd::from(file))
        .wait()
        .map_err(|e| Error::Usb(format!("usb device: {e}")))
}

/// Why an interface class rules a device out as a phone. Probing is not
/// passive on the phone's side: a device that answers the vendor request
/// wrongly can misbehave, and the scan comes round on every plug, so a
/// keyboard, a hub, a sound card or a Bluetooth radio is never asked. A
/// composite device reports class 0 on its device descriptor and the real
/// classes on its interfaces, so those are what is looked at.
pub(crate) fn not_a_phone(class: u8, subclass: u8, protocol: u8) -> Option<&'static str> {
    match class {
        0x01 => Some("audio"),
        0x03 => Some("hid"),
        0x07 => Some("printer"),
        0x09 => Some("hub"),
        0x0b => Some("smart card"),
        0x0e => Some("video"),
        // 0xe0 also covers RNDIS tethering, so only Bluetooth radios
        // (subclass 1, protocol 1) are excluded, not the whole class.
        0xe0 if subclass == 1 && protocol == 1 => Some("bluetooth"),
        _ => None,
    }
}

fn candidate(dev: &nusb::Device) -> Option<&'static str> {
    let Ok(config) = dev.active_configuration() else {
        // Cannot tell; the protocol request decides.
        return None;
    };
    for intf in config.interfaces() {
        for alt in intf.alt_settings() {
            if let Some(why) = not_a_phone(alt.class(), alt.subclass(), alt.protocol()) {
                return Some(why);
            }
        }
    }
    None
}

/// Ask the device whether it speaks the accessory protocol and, if so,
/// hand it the six strings and start accessory mode. On `Switched` the
/// device is gone (re-enumerating) by the time this returns; the phone
/// may first show a consent dialog, so the accessory can take a while.
pub fn switch_to_accessory(device: &lehu_usb::Device) -> Result<Switch, Error> {
    let dev = open_node(device)?;
    if let Some(why) = candidate(&dev) {
        return Ok(Switch::Skipped(why));
    }
    let reply = dev
        .control_in(
            ControlIn {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: AOAP_REQ_GET_PROTOCOL,
                value: 0,
                index: 0,
                length: 2,
            },
            CONTROL_TIMEOUT,
        )
        .wait();
    let version = match reply {
        Ok(bytes) if bytes.len() >= 2 => u16::from_le_bytes([bytes[0], bytes[1]]),
        Ok(_) => return Ok(Switch::NotSupported),
        Err(e) => {
            // A device without the request stalls it; that is the common
            // "not a phone" answer, not an error.
            log::debug!("aoap: {device}: get protocol: {e}");
            return Ok(Switch::NotSupported);
        }
    };
    if version < 1 {
        return Ok(Switch::NotSupported);
    }
    log::info!("aoap: {device}: accessory protocol {version}, switching");
    let mut text = Vec::with_capacity(64);
    for (index, s) in AOAP_STRINGS {
        text.clear();
        text.extend_from_slice(s.as_bytes());
        text.push(0);
        vendor_out(&dev, AOAP_REQ_SEND_STRING, index, &text)
            .map_err(|e| Error::Usb(format!("sending string {index}: {e}")))?;
    }
    vendor_out(&dev, AOAP_REQ_START, 0, &[]).map_err(|e| Error::Usb(format!("start: {e}")))?;
    Ok(Switch::Switched)
}

fn vendor_out(dev: &nusb::Device, request: u8, index: u16, data: &[u8]) -> Result<(), String> {
    dev.control_out(
        ControlOut {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request,
            value: 0,
            index,
            data,
        },
        CONTROL_TIMEOUT,
    )
    .wait()
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_six_strings_name_the_project() {
        let by_index: Vec<u16> = AOAP_STRINGS.iter().map(|(i, _)| *i).collect();
        assert_eq!(by_index, [0, 1, 2, 3, 4, 5]);
        assert_eq!(AOAP_STRINGS[0].1, "Android");
        assert_eq!(AOAP_STRINGS[1].1, "Android Auto");
        assert!(AOAP_STRINGS[2].1.contains("LEHU"));
        assert_eq!(AOAP_STRINGS[4].1, "https://github.com/mbt28/lehu");
        assert_eq!(AOAP_STRINGS[5].1, "LEHU");
        for (_, s) in AOAP_STRINGS {
            assert!(!s.is_empty() && !s.contains('\0'));
        }
    }

    #[test]
    fn devices_that_are_never_phones() {
        assert_eq!(not_a_phone(0x03, 1, 1), Some("hid"));
        assert_eq!(not_a_phone(0x09, 0, 0), Some("hub"));
        assert_eq!(not_a_phone(0xe0, 1, 1), Some("bluetooth"));
        assert_eq!(not_a_phone(0xe0, 1, 3), None, "RNDIS tethering is a phone");
        assert_eq!(not_a_phone(0xff, 0xff, 0), None, "vendor-specific: MTP/ADB");
        assert_eq!(not_a_phone(0x06, 1, 1), None, "PTP is a phone");
    }

    #[test]
    fn a_missing_node_is_an_open_error() {
        let d = lehu_usb::Device::new(250, 250, lehu_usb::Id::default(), 0, "none");
        assert!(matches!(switch_to_accessory(&d), Err(Error::Open(_))));
    }
}
