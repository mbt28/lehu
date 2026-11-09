// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
// Copyright (C) 2025-2026 Lasse Heitgres (LIVI, https://github.com/f-io/LIVI)
//! What the head unit tells the phone about itself in `GET /info`, and the
//! commands it sends over the event channel. The info plist is the part of
//! the protocol the phone judges whole: a missing view area, an icon, an
//! audio format set it dislikes, and it tears the session down right after
//! RECORD without a word.
//!
//! The info plist follows LIVI's cp/stack/getInfo.ts and livi-cp/src/info.rs
//! (Lasse Heitgres, GPL-3.0), through FastCarPlay's cp_av.cpp; changed for
//! LEHU.

use lehu_core::settings::NightMode;

use crate::Config;
use crate::hid;
use crate::plist::{self, Value};

pub const MAIN_DISPLAY_UUID: &str = "b7e6c5a0-1111-4000-8000-000000000001";
pub const PLIST_CT: &str = "application/x-apple-binary-plist";
pub const SOURCE_VERSION: &str = "280.33.8";

/// The CarPlay home-screen icon, the LEHU badge.
const ICON_120: &[u8] = include_bytes!("../assets/lehu-120.png");
const ICON_180: &[u8] = include_bytes!("../assets/lehu-180.png");

pub const STREAM_MAIN_SCREEN: i64 = 110;
pub const STREAM_ALT_SCREEN: i64 = 111;
pub const STREAM_MAIN_AUDIO: i64 = 100;
pub const STREAM_ALT_AUDIO: i64 = 101;
pub const STREAM_MAIN_HIGH_AUDIO: i64 = 102;
pub const STREAM_DATA: i64 = 130;

// The audio-format bits. No 44.1 kHz family anywhere: this board's codec
// is 48 kHz-native and the phone prefers 44.1 when both are offered,
// which forced a fractional resample and a capture rate the driver
// refuses next to 48 kHz playback. 8 to 32 kHz and 48 kHz all divide 48.
const PCM: i64 = 0x3fc | 0xc000;
const PCM_MONO: i64 = 0x154 | 0x4000;
/// Opus at 16, 24 and 48 kHz mono, output.
const OPUS: i64 = 0x7000_0000;
/// Opus at 16 and 24 kHz only for the microphone: both are SILK-only,
/// cheap on a core without a float unit; 48 kHz is the hybrid mode and
/// too heavy to encode live here. Offering 24 only once stopped telephony
/// from negotiating at all, so both stay.
const OPUS_VOICE: i64 = 0x3000_0000;
/// AAC-LC at 48 kHz for media; iOS resamples its content to it.
const AAC48: i64 = 0x80_0000;

/// The appearance the phone is asked for: it darkens by itself at sunset,
/// the car can only add night. Mode 0 light, 1 dark; setting 1 is the
/// driver's choice (Day stays light after sunset), 0 automatic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Appearance {
    pub mode: i64,
    pub setting: i64,
    pub night: bool,
}

pub fn appearance(night: NightMode) -> Appearance {
    match night {
        NightMode::Night => Appearance {
            mode: 1,
            setting: 1,
            night: true,
        },
        NightMode::Day => Appearance {
            mode: 0,
            setting: 1,
            night: false,
        },
        NightMode::Auto => Appearance {
            mode: 0,
            setting: 0,
            night: false,
        },
    }
}

/// The main display's uuid: in a CarPlay Ultra session with a theme, the
/// theme's centre display, so the phone matches its artwork to it.
pub fn main_uuid(cfg: &Config) -> &str {
    cfg.ultra_theme()
        .map_or(MAIN_DISPLAY_UUID, |t| t.center.as_str())
}

/// The two commands that change the appearance of a running session.
pub fn appearance_commands(display: &str, night: NightMode) -> [Vec<u8>; 2] {
    let a = appearance(night);
    let update = Value::dict()
        .with("type", Value::str("uiAppearanceUpdate"))
        .with(
            "params",
            Value::dict()
                .with("uuid", Value::str(display))
                .with("appearanceMode", Value::Int(a.mode))
                .with("appearanceSetting", Value::Int(a.setting)),
        );
    let set_night = Value::dict().with("type", Value::str("setNightMode")).with(
        "params",
        Value::dict().with("nightMode", Value::Bool(a.night)),
    );
    [plist::encode(&update), plist::encode(&set_night)]
}

pub fn hid_command(uuid: &str, report: &[u8]) -> Vec<u8> {
    plist::encode(
        &Value::dict()
            .with("type", Value::str("hidSendReport"))
            .with("uuid", Value::str(uuid))
            .with("hidReport", Value::Data(report.to_vec())),
    )
}

pub fn siri_command(action: i64) -> Vec<u8> {
    plist::encode(&Value::dict().with("type", Value::str("requestSiri")).with(
        "params",
        Value::dict().with("siriAction", Value::Int(action)),
    ))
}

pub fn force_keyframe_command(display: &str) -> Vec<u8> {
    plist::encode(
        &Value::dict()
            .with("type", Value::str("forceKeyFrame"))
            .with("params", Value::dict().with("uuid", Value::str(display))),
    )
}

fn audio_format(kind: i64, usage: &str, out: i64, input: Option<i64>) -> Value {
    let mut d = Value::dict()
        .with("type", Value::Int(kind))
        .with("audioType", Value::str(usage))
        .with("audioOutputFormats", Value::Int(out));
    if let Some(i) = input {
        d.set("audioInputFormats", Value::Int(i));
    }
    d
}

fn latency(kind: i64, usage: Option<&str>) -> Value {
    let mut d = Value::dict()
        .with("type", Value::Int(kind))
        .with("inputLatencyMicros", Value::Int(0))
        .with("outputLatencyMicros", Value::Int(0));
    if let Some(u) = usage {
        d.set("audioType", Value::str(u));
    }
    d
}

fn resource(id: i64) -> Value {
    Value::dict()
        .with("resourceID", Value::Int(id))
        .with("transferType", Value::Int(1))
        .with("transferPriority", Value::Int(100))
        .with("takeConstraint", Value::Int(100))
        .with("borrowConstraint", Value::Int(100))
        .with("unborrowConstraint", Value::Int(100))
}

fn hid_device(display: &str, uuid: &str, name: &str, descriptor: &[u8]) -> Value {
    Value::dict()
        .with("hidProductID", Value::Int(1))
        .with("hidVendorID", Value::Int(2))
        .with("hidCountryCode", Value::Int(0))
        .with("uuid", Value::str(uuid))
        .with("name", Value::str(name))
        .with("displayUUID", Value::str(display))
        .with("hidDescriptor", Value::Data(descriptor.to_vec()))
}

fn icon(png: &[u8], px: i64) -> Value {
    Value::dict()
        .with("imageData", Value::Data(png.to_vec()))
        .with("widthPixels", Value::Int(px))
        .with("heightPixels", Value::Int(px))
        .with("prerendered", Value::Bool(true))
}

/// The capabilities, as a binary plist.
pub fn info_plist(cfg: &Config, night: NightMode) -> Vec<u8> {
    plist::encode(&info_value(cfg, night))
}

pub fn info_value(cfg: &Config, night: NightMode) -> Value {
    let a = appearance(night);
    let main = main_uuid(cfg);
    let (w, h) = (i64::from(cfg.width), i64::from(cfg.height));
    let area = |safe: Option<Value>| {
        let mut v = Value::dict()
            .with("widthPixels", Value::Int(w))
            .with("heightPixels", Value::Int(h))
            .with("originXPixels", Value::Int(0))
            .with("originYPixels", Value::Int(0));
        match safe {
            Some(s) => v.set("safeArea", s),
            None => v.set("drawUIOutsideSafeArea", Value::Bool(true)),
        };
        v
    };
    // The view-areas feature is claimed, so the display carries its view
    // area geometry; claiming it without the geometry ends the session.
    let display = Value::dict()
        .with("uuid", Value::str(main))
        .with("type", Value::Int(STREAM_MAIN_SCREEN))
        .with("maxFPS", Value::Int(i64::from(cfg.fps)))
        .with("widthPixels", Value::Int(w))
        .with("heightPixels", Value::Int(h))
        .with("widthPhysical", Value::Int(i64::from(cfg.width_mm)))
        .with("heightPhysical", Value::Int(i64::from(cfg.height_mm)))
        .with("features", Value::Int(0x08 | 0x02))
        .with("primaryInputDevice", Value::Int(3))
        .with("uiAppearanceMode", Value::Int(a.mode))
        .with("uiAppearanceSetting", Value::Int(a.setting))
        .with("viewAreas", Value::Array(vec![area(Some(area(None)))]))
        .with("initialViewArea", Value::Int(0));

    // The phone validates the whole array: narrowing the PCM set or
    // dropping Opus from the output made it refuse the media stream and
    // telephony both. Only the input (the microphone) is narrowed.
    let audio_formats = Value::Array(vec![
        audio_format(100, "compatibility", PCM, Some(PCM_MONO)),
        audio_format(101, "compatibility", PCM, None),
        audio_format(100, "default", PCM | OPUS, Some(PCM_MONO | OPUS_VOICE)),
        audio_format(100, "alert", PCM | OPUS, None),
        audio_format(100, "media", PCM, None),
        audio_format(
            100,
            "telephony",
            PCM_MONO | OPUS,
            Some(PCM_MONO | OPUS_VOICE),
        ),
        audio_format(
            100,
            "speechRecognition",
            PCM_MONO | OPUS,
            Some(PCM_MONO | OPUS_VOICE),
        ),
        audio_format(101, "default", PCM | OPUS, None),
        audio_format(102, "media", AAC48, None),
    ]);
    let audio_latencies = Value::Array(vec![
        latency(100, None),
        latency(100, Some("default")),
        latency(100, Some("media")),
        latency(100, Some("telephony")),
        latency(100, Some("speechRecognition")),
        latency(100, Some("alert")),
        latency(101, None),
        latency(101, Some("default")),
        latency(102, Some("default")),
    ]);
    let modes = Value::dict()
        .with("resources", Value::Array(vec![resource(1), resource(2)]))
        .with(
            "appStates",
            Value::Array(vec![
                Value::dict()
                    .with("appStateID", Value::Int(2))
                    .with("state", Value::Bool(false)),
                Value::dict()
                    .with("appStateID", Value::Int(1))
                    .with("speechMode", Value::Int(-1)),
                Value::dict()
                    .with("appStateID", Value::Int(3))
                    .with("state", Value::Bool(false)),
            ]),
        );
    let touch = hid::touch_descriptor(cfg.width as u16, cfg.height as u16);
    let hid_devices = Value::Array(vec![
        hid_device(main, hid::TOUCH_UUID, "LEHU Touchscreen", &touch),
        hid_device(main, hid::KNOB_UUID, "LEHU Knob", hid::KNOB_DESCRIPTOR),
        hid_device(main, hid::MEDIA_UUID, "LEHU Media", hid::MEDIA_DESCRIPTOR),
        hid_device(
            main,
            hid::TELEPHONY_UUID,
            "LEHU Telephony",
            hid::TELEPHONY_DESCRIPTOR,
        ),
    ]);
    let mut info = Value::dict()
        .with("sourceVersion", Value::str(SOURCE_VERSION))
        .with("features", Value::Int(0x61_5653_aee2))
        .with("statusFlags", Value::Int(4))
        .with("model", Value::str(&cfg.model))
        .with("manufacturer", Value::str(&cfg.manufacturer))
        .with("deviceID", Value::str(&cfg.name))
        .with("name", Value::str(&cfg.name))
        .with("rightHandDrive", Value::Bool(cfg.right_hand_drive))
        .with("keepAliveLowPower", Value::Bool(true))
        .with("keepAliveSendStatsAsBody", Value::Bool(false))
        .with("modes", modes)
        .with("audioLatencies", audio_latencies)
        .with("audioFormats", audio_formats)
        .with(
            "extendedFeatures",
            Value::Array(vec![
                Value::str("vocoderInfo"),
                Value::str("enhancedRequestCarUI"),
            ]),
        )
        .with("displays", Value::Array(vec![display]))
        .with("nightMode", Value::Bool(a.night))
        .with("hidDevices", hid_devices)
        .with(
            "bluetoothIDs",
            Value::Array(vec![Value::str("2c:cf:67:fb:12:de")]),
        )
        // The icon on the CarPlay home screen: without one there is
        // nothing to place and the phone ends the session after RECORD.
        .with("oemIconVisible", Value::Bool(true))
        .with("oemIconLabel", Value::str(&cfg.name))
        .with(
            "oemIcons",
            Value::Array(vec![icon(ICON_120, 120), icon(ICON_180, 180)]),
        );
    if cfg.codec.is_hevc() {
        info.set("hevcInfo", Value::dict());
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use lehu_video::Codec;

    #[test]
    fn the_info_round_trips_with_what_the_phone_insists_on() {
        let cfg = Config::for_test(Codec::H264);
        let bytes = info_plist(&cfg, NightMode::Auto);
        let v = plist::decode(&bytes).expect("decodes");
        assert_eq!(v.str_or("name", ""), "LEHU");
        assert_eq!(v.str_or("oemIconLabel", ""), "LEHU");
        assert_eq!(v.get("hevcInfo"), None);
        let formats = v.get("audioFormats").and_then(Value::as_array).unwrap();
        assert_eq!(formats.len(), 9);
        let media = formats.iter().find(|f| f.int_or("type", 0) == 102).unwrap();
        assert_eq!(media.int_or("audioOutputFormats", 0), AAC48);
        let tel = formats
            .iter()
            .find(|f| f.str_or("audioType", "") == "telephony")
            .unwrap();
        assert_eq!(tel.int_or("audioInputFormats", 0), PCM_MONO | OPUS_VOICE);
        assert_eq!(tel.int_or("audioOutputFormats", 0) & OPUS, OPUS);
        let display = &v.get("displays").and_then(Value::as_array).unwrap()[0];
        assert_eq!(display.int_or("widthPixels", 0), 800);
        let areas = display.get("viewAreas").and_then(Value::as_array).unwrap();
        assert!(areas[0].get("safeArea").is_some());
        let icons = v.get("oemIcons").and_then(Value::as_array).unwrap();
        assert_eq!(icons.len(), 2);
        assert!(
            icons[0]
                .get("imageData")
                .and_then(Value::as_data)
                .unwrap()
                .starts_with(b"\x89PNG")
        );
        let hids = v.get("hidDevices").and_then(Value::as_array).unwrap();
        assert_eq!(hids.len(), 4);
        let hevc = info_value(&Config::for_test(Codec::Hevc), NightMode::Night);
        assert!(hevc.get("hevcInfo").is_some());
        assert_eq!(hevc.bool_of("nightMode"), Some(true));
    }

    #[test]
    fn an_ultra_theme_names_the_screen() {
        let mut cfg = Config::for_test(Codec::H264);
        cfg.theme = Some(crate::ultra::Theme {
            asset: "A".into(),
            version: 1,
            sdk: "19.0".into(),
            center: "Center_Display".into(),
            layout: None,
        });
        assert_eq!(main_uuid(&cfg), MAIN_DISPLAY_UUID, "not an Ultra session");
        cfg.ultra = true;
        assert_eq!(main_uuid(&cfg), "Center_Display");
        let v = info_value(&cfg, NightMode::Auto);
        let display = &v.get("displays").and_then(Value::as_array).unwrap()[0];
        assert_eq!(display.str_or("uuid", ""), "Center_Display");
        let hids = v.get("hidDevices").and_then(Value::as_array).unwrap();
        assert!(
            hids.iter()
                .all(|h| h.str_or("displayUUID", "") == "Center_Display")
        );
    }

    #[test]
    fn commands_decode() {
        let [update, night] = appearance_commands(MAIN_DISPLAY_UUID, NightMode::Night);
        let u = plist::decode(&update).unwrap();
        assert_eq!(u.str_or("type", ""), "uiAppearanceUpdate");
        assert_eq!(u.get("params").unwrap().int_or("appearanceMode", 0), 1);
        let n = plist::decode(&night).unwrap();
        assert_eq!(n.get("params").unwrap().bool_of("nightMode"), Some(true));
        let k = plist::decode(&force_keyframe_command(MAIN_DISPLAY_UUID)).unwrap();
        assert_eq!(k.str_or("type", ""), "forceKeyFrame");
        assert_eq!(
            k.get("params").unwrap().str_or("uuid", ""),
            MAIN_DISPLAY_UUID
        );
        let h = plist::decode(&hid_command(hid::KNOB_UUID, &[1, 2])).unwrap();
        assert_eq!(
            h.get("hidReport").and_then(Value::as_data),
            Some(&[1u8, 2][..])
        );
        assert_eq!(
            plist::decode(&siri_command(2))
                .unwrap()
                .get("params")
                .unwrap()
                .int_or("siriAction", 0),
            2
        );
    }
}
