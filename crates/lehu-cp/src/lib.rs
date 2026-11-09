// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! CarPlay: the triggers that bring an iPhone to the head unit and the AV
//! stack the phone then connects to, driven by the main loop as a
//! `lehu_session::Session`.
//!
//! Two halves, joined by the session:
//!
//! - The trigger (`wired`): the phone is switched to configuration 6,
//!   usbmux opened on it, lockdown paired and asked for the carkit
//!   service, iAP2 negotiated over it, the head unit identified and
//!   authenticated with the MFi coprocessor, and the phone handed the
//!   link-local address of the USB network and the control port. Threads
//!   and blocking reads with timeouts, no runtime.
//! - Its sibling, the wireless bootstrap (`wireless`, `mdns`): the phone
//!   finds the iAP2 and CarPlay services in the head unit's Bluetooth
//!   SDP, is identified and authenticated the same way over RFCOMM, and
//!   handed the access point's credentials and our address on it; the
//!   control server is announced over mDNS as an AirPlay receiver.
//! - The stack (`server`, `control`, `av`): the phone connects to the
//!   control port, pairs or verifies, the channel turns to sealed frames,
//!   and the AV session negotiates its streams: video over TCP as Annex-B
//!   access units into the decoder's queue, audio over RTP to the mixer's
//!   sink still encoded, the microphone back, the event channel for the
//!   phone's requests and our input. Nothing here is transport-specific:
//!   both triggers hand the same stack their peer.

pub mod av;
pub mod carplay_msg;
pub mod cipher;
pub mod control;
pub mod crypto;
pub mod file;
pub mod hid;
pub mod iap2;
pub mod info;
pub mod lockdown;
pub mod mdns;
pub mod media;
pub mod mfi;
pub mod mfi4;
pub mod nalu;
pub mod ncm;
pub mod opack;
pub mod pairing;
pub mod plist;
pub mod rtsp;
pub mod server;
pub mod session;
pub mod srp;
pub mod state;
pub mod tlv8;
pub mod trigger;
pub mod ui_sync;
pub mod ultra;
pub mod usbmux;
pub mod vehicle;
pub mod wired;
pub mod wireless;

use std::path::PathBuf;

use lehu_core::settings::{NightMode, Settings};
use lehu_video::{Caps, Codec};

pub use session::{Error, Session};

/// What a session needs to know at start. Taken from the settings and the
/// display once; nothing changes under a live session (the phone is told
/// the screen size at setup).
#[derive(Clone, Debug)]
pub struct Config {
    /// The name the phone shows: the icon's label, the device id.
    pub name: String,
    pub model: String,
    pub manufacturer: String,
    /// The picture asked of the phone, in pixels and millimetres. CarPlay
    /// takes literal sizes, so the panel's own and the display engine never
    /// rescales a frame.
    pub width: u32,
    pub height: u32,
    pub width_mm: u32,
    pub height_mm: u32,
    pub fps: u32,
    /// The codec offered: HEVC only when the board decodes it.
    pub codec: Codec,
    pub night: NightMode,
    pub right_hand_drive: bool,
    pub mfi_bus: String,
    pub mfi_addr: u8,
    /// Where the identity, the paired phones and the lockdown records live.
    pub state_dir: PathBuf,
    /// The control port the phone is told to connect to.
    pub port: u16,
    /// One access unit's buffer: a frame bigger than this is dropped.
    pub unit_bytes: usize,
    /// Real-time priority for the audio threads.
    pub realtime: bool,
    /// CarPlay Ultra offered to the phone. The setting asks for it; the
    /// session start clears it when the chip is not an MFi 3.0, which
    /// alone answers the mutual authentication.
    pub ultra: bool,
    /// The car-maker theme for Ultra, provisioned offline in the state
    /// directory; read when the session starts.
    pub theme: Option<ultra::Theme>,
}

impl Config {
    /// The 800x480 panel is 154 mm wide: when a display reports no
    /// physical size one is made at that density, so the phone's UI keeps
    /// the size it has always had.
    const DEFAULT_WIDTH_MM: u32 = 154;

    pub fn from_settings(
        s: &Settings,
        codec: Codec,
        width: u32,
        height: u32,
        width_mm: u32,
        height_mm: u32,
    ) -> Self {
        let synth = |px: u32| px * Self::DEFAULT_WIDTH_MM / 800;
        Self {
            name: "LEHU".into(),
            model: "LEHU1,1".into(),
            manufacturer: "LEHU".into(),
            width: s.display.width.unwrap_or(width).max(1),
            height: s.display.height.unwrap_or(height).max(1),
            width_mm: if width_mm > 0 { width_mm } else { synth(width) },
            height_mm: if height_mm > 0 {
                height_mm
            } else {
                synth(height)
            },
            fps: s.display.fps.max(1),
            codec,
            night: s.display.night_mode,
            right_hand_drive: !s.display.left_hand_drive,
            mfi_bus: s.carplay.mfi_i2c_bus.clone(),
            mfi_addr: s.carplay.mfi_i2c_addr as u8,
            state_dir: PathBuf::from("/var/lib/lehu"),
            port: 7000,
            unit_bytes: 512 * 1024,
            realtime: s.system.realtime_priority,
            ultra: s.carplay.ultra,
            theme: None,
        }
    }

    /// Keep the picture within what the codec's decoder takes: scaled
    /// down, aspect kept, when the panel (or `display.width`/`height`) is
    /// larger. The display front end scales it back up to the panel.
    pub fn fit_decoder(&mut self, caps: &Caps) {
        let Some((max_w, max_h)) = caps.max(self.codec) else {
            return;
        };
        if caps.fits(self.codec, self.width, self.height) {
            return;
        }
        let (w0, h0) = (u64::from(self.width), u64::from(self.height));
        let mut w = u64::from(max_w).min(w0);
        let mut h = h0 * w / w0;
        // Decoders count whole macroblocks: the height is checked coded.
        if h.div_ceil(16) * 16 > u64::from(max_h) {
            h = u64::from(max_h) & !15;
            w = w0 * h / h0;
        }
        let even = |v: u64| (u32::try_from(v).unwrap_or(u32::MAX) & !1).max(2);
        let (w, h) = (even(w), even(h));
        log::warn!(
            "carplay: {}x{} is beyond the {} decoder ({max_w}x{max_h}); asking for {w}x{h}",
            self.width,
            self.height,
            self.codec
        );
        self.width = w;
        self.height = h;
    }

    /// What the start message says of Ultra.
    pub fn ultra_start(&self) -> carplay_msg::UltraStart<'_> {
        carplay_msg::UltraStart {
            mutual_auth: self.ultra,
            theme: self.ultra_theme(),
        }
    }

    /// The theme, in an Ultra session.
    pub fn ultra_theme(&self) -> Option<&ultra::Theme> {
        self.theme.as_ref().filter(|_| self.ultra)
    }

    /// The theme's asset, named in the identification.
    pub fn theme_asset(&self) -> Option<&str> {
        self.ultra_theme().map(|t| t.asset.as_str())
    }

    /// A configuration for the tests: the default panel, no devices.
    pub fn for_test(codec: Codec) -> Self {
        let dir = std::env::temp_dir().join(format!("lehu-cp-{}", std::process::id()));
        Self {
            name: "LEHU".into(),
            model: "LEHU1,1".into(),
            manufacturer: "LEHU".into(),
            width: 800,
            height: 480,
            width_mm: 154,
            height_mm: 90,
            fps: 60,
            codec,
            night: NightMode::Auto,
            right_hand_drive: false,
            mfi_bus: String::new(),
            mfi_addr: 0,
            state_dir: dir,
            port: 0,
            unit_bytes: 64 * 1024,
            realtime: false,
            ultra: false,
            theme: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lehu_core::settings::VideoCodec;
    use lehu_video::{Ffmpeg, Format, Hardware};

    fn decoder(format: Format, max: Option<(u32, u32)>) -> Caps {
        let mut caps = Caps::none();
        caps.add_hardware(
            format,
            Hardware {
                node: "/dev/video0".into(),
                driver: "test".into(),
                stateless: true,
                max,
            },
        );
        caps.set_ffmpeg(format, Ffmpeg::all());
        caps
    }

    #[test]
    fn the_picture_stays_within_the_decoder() {
        // The F1C200s: H.264 on cedrus, the 800x480 panel as it is.
        let f1c = decoder(Format::H264, Some((1920, 1088)));
        let codec = lehu_video::carplay_codec(&f1c, VideoCodec::Auto);
        assert_eq!(codec, Codec::H264);
        let mut c = Config::for_test(codec);
        c.fit_decoder(&f1c);
        assert_eq!((c.width, c.height), (800, 480));
        // A 1920x1200 panel: 1088 lines at most, the aspect kept.
        c.width = 1920;
        c.height = 1200;
        c.fit_decoder(&f1c);
        assert_eq!((c.width, c.height), (1740, 1088));
        // Too wide: the width gives way.
        c.width = 2560;
        c.height = 720;
        c.fit_decoder(&f1c);
        assert_eq!((c.width, c.height), (1920, 540));
        // No size from the driver, or no decoder: nothing to fit to.
        let any = decoder(Format::Hevc, None);
        let mut c = Config::for_test(Codec::Hevc);
        c.width = 3840;
        c.height = 2160;
        c.fit_decoder(&any);
        assert_eq!((c.width, c.height), (3840, 2160));
        c.fit_decoder(&Caps::none());
        assert_eq!((c.width, c.height), (3840, 2160));
    }
}
