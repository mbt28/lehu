// SPDX-License-Identifier: GPL-3.0-or-later
//! FastCarPlay's `settings.txt` and `usersettings.txt`, read once into a
//! LEHU user file: flat `key = value` lines, `#` comments, a few renamed
//! keys and two integers that became words. Only the keys those files set
//! are written, so the result layers over LEHU's defaults the way the old
//! files layered over FastCarPlay's.

use toml::{Table, Value};

use super::merge;

/// What an old key becomes: a dotted LEHU key and the type of its value.
#[derive(Clone, Copy)]
enum Kind {
    Str,
    Int,
    Bool,
    Float,
    NightMode,
    LogLevel,
    /// -1 meant "not set"; it becomes an absent key.
    Volume,
}

const MAP: &[(&str, &str, Kind)] = &[
    ("aa-vendor-id", "android-auto.vendor-id", Kind::Int),
    ("aa-product-id", "android-auto.product-id", Kind::Int),
    ("aa-resolution", "android-auto.resolution", Kind::Int),
    ("android-resolution", "android-auto.resolution", Kind::Int),
    ("aa-video-fps", "android-auto.fps", Kind::Int),
    ("android-dpi", "android-auto.dpi", Kind::Int),
    (
        "aa-accessory-timeout",
        "android-auto.accessory-timeout-ms",
        Kind::Int,
    ),
    ("aa-max-unacked", "android-auto.max-unacked", Kind::Int),
    ("force-chacha20", "android-auto.force-chacha20", Kind::Bool),
    ("wifi-interface", "wifi.interface", Kind::Str),
    ("hostapd-conf", "wifi.hostapd-conf", Kind::Str),
    ("ap-restart-command", "wifi.ap-restart-command", Kind::Str),
    ("wifi-ssid", "wifi.ssid", Kind::Str),
    ("wifi-passphrase", "wifi.passphrase", Kind::Str),
    ("wifi-channel", "wifi.channel", Kind::Int),
    ("wifi-ap-ip", "wifi.ap-ip", Kind::Str),
    ("bluetooth-name", "bluetooth.name", Kind::Str),
    ("mfi-i2c-bus", "carplay.mfi-i2c-bus", Kind::Str),
    ("mfi-i2c-addr", "carplay.mfi-i2c-addr", Kind::Int),
    ("width", "display.width", Kind::Int),
    ("height", "display.height", Kind::Int),
    ("source-fps", "display.fps", Kind::Int),
    ("video-path", "display.video-path", Kind::Str),
    ("night-mode", "display.night-mode", Kind::NightMode),
    ("left-hand-drive", "display.left-hand-drive", Kind::Bool),
    ("log-level", "system.log-level", Kind::LogLevel),
    ("ui", "system.ui", Kind::Bool),
    ("lvgl-ui", "system.ui", Kind::Bool),
    ("realtime-priority", "system.realtime-priority", Kind::Bool),
    ("on-connect-script", "system.on-connect-script", Kind::Str),
    (
        "on-disconnect-script",
        "system.on-disconnect-script",
        Kind::Str,
    ),
    ("reboot-command", "system.reboot-command", Kind::Str),
    ("shutdown-command", "system.shutdown-command", Kind::Str),
    ("touch-device", "touch.device", Kind::Str),
    ("touch-swap-xy", "touch.swap-xy", Kind::Bool),
    ("touch-invert-x", "touch.invert-x", Kind::Bool),
    ("touch-invert-y", "touch.invert-y", Kind::Bool),
    ("rendering-buffer", "decode.rendering-buffer", Kind::Int),
    ("decode-low-delay", "decode.low-delay", Kind::Bool),
    ("decode-fast", "decode.fast", Kind::Bool),
    ("async-usb-calls", "usb.async-calls", Kind::Int),
    ("usb-buffer-size", "usb.transfer-size", Kind::Int),
    ("usb-buffer", "usb.buffer", Kind::Int),
    ("usb-write-timeout-ms", "usb.write-timeout-ms", Kind::Int),
    ("usb-write-retries", "usb.write-retries", Kind::Int),
    (
        "usb-write-retry-delay-ms",
        "usb.write-retry-delay-ms",
        Kind::Int,
    ),
    ("alsa-device", "audio.device", Kind::Str),
    ("volume", "audio.volume", Kind::Volume),
    ("volume-control", "audio.volume-control", Kind::Str),
    ("volume-device", "audio.volume-device", Kind::Str),
    ("volume-step", "audio.volume-step", Kind::Int),
    ("audio-buffer-ms", "audio.buffer-ms", Kind::Int),
    (
        "audio-buffer-wait-call",
        "audio.call-prefill-segments",
        Kind::Int,
    ),
    ("audio-fade", "audio.fade", Kind::Float),
    (
        "audio-voice-max-latency-ms",
        "audio.voice-max-latency-ms",
        Kind::Int,
    ),
    ("audio-idle-hold-ms", "audio.idle-hold-ms", Kind::Int),
    ("mic-gain", "audio.mic-gain", Kind::Int),
    ("camera-device", "camera.device", Kind::Str),
    ("camera-input", "camera.input", Kind::Int),
    ("camera-key", "camera.key", Kind::Int),
    ("camera-fastswitch", "camera.fast-switch", Kind::Bool),
];

/// Picture levels of a `camera-N` profile string, in their old order.
const CAMERA_LEVELS: &[&str] = &[
    "brightness",
    "contrast",
    "saturation",
    "hue",
    "sharpness",
    "chroma-gain",
    "chroma-enhance",
];

pub struct Import {
    /// The user file to write: only what the old files set.
    pub table: Table,
    /// Lines that did not convert, one each, with the file they came from.
    pub warnings: Vec<String>,
}

/// `files` are (name, content) in the order they applied: the preset, then
/// the user's overrides.
pub fn from_fastcarplay(files: &[(&str, &str)]) -> Import {
    let mut table = Table::new();
    let mut warnings = Vec::new();
    let mut camera_inputs: Vec<Option<Table>> = vec![None, None, None, None];

    for (name, text) in files {
        for (number, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                warnings.push(format!("{name}:{}: no `=`: {line}", number + 1));
                continue;
            };
            let key = key.trim();
            let value = strip_comment(value).trim();

            if let Some(index) = key
                .strip_prefix("camera-")
                .and_then(|n| n.parse::<usize>().ok())
                && index < 4
            {
                camera_inputs[index] = Some(camera_profile(value));
                continue;
            }
            let Some(&(_, new_key, kind)) = MAP.iter().find(|(old, _, _)| *old == key) else {
                warnings.push(format!(
                    "{name}:{}: `{key}` has no LEHU setting",
                    number + 1
                ));
                continue;
            };
            match convert(value, kind) {
                Ok(Some(v)) => set(&mut table, new_key, v),
                Ok(None) => {}
                Err(why) => {
                    warnings.push(format!("{name}:{}: `{key} = {value}`: {why}", number + 1))
                }
            }
        }
    }

    if let Some(last) = camera_inputs.iter().rposition(Option::is_some) {
        let inputs: Vec<Value> = camera_inputs
            .into_iter()
            .take(last + 1)
            .map(|t| Value::Table(t.unwrap_or_default()))
            .collect();
        set(&mut table, "camera.inputs", Value::Array(inputs));
    }
    Import { table, warnings }
}

fn strip_comment(value: &str) -> &str {
    match value.find(" #") {
        Some(i) => &value[..i],
        None => value,
    }
}

fn set(table: &mut Table, key: &str, value: Value) {
    let mut layer = Table::new();
    let mut cursor = &mut layer;
    let mut parts = key.split('.').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            cursor.insert(part.to_string(), value);
            break;
        }
        cursor = cursor
            .entry(part.to_string())
            .or_insert_with(|| Value::Table(Table::new()))
            .as_table_mut()
            .expect("a fresh table");
    }
    merge(table, layer);
}

fn convert(value: &str, kind: Kind) -> Result<Option<Value>, String> {
    Ok(Some(match kind {
        Kind::Str => Value::String(value.to_string()),
        Kind::Int => Value::Integer(int(value)?),
        Kind::Bool => Value::Boolean(match value.to_ascii_lowercase().as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return Err("expected true/false or 1/0".into()),
        }),
        Kind::Float => Value::Float(value.parse::<f64>().map_err(|_| "expected a number")?),
        Kind::NightMode => Value::String(
            match int(value)? {
                0 => "day",
                1 => "night",
                2 => "auto",
                _ => return Err("expected 0 (day), 1 (night) or 2 (auto)".into()),
            }
            .into(),
        ),
        Kind::LogLevel => Value::String(
            match int(value)? {
                0 | 1 => "error",
                2 => "warn",
                3 => "info",
                4 => "debug",
                5 => "verbose",
                6 => "protocol",
                _ => return Err("expected a level 0..=6".into()),
            }
            .into(),
        ),
        Kind::Volume => {
            let v = int(value)?;
            if v < 0 {
                return Ok(None);
            }
            Value::Integer(v)
        }
    }))
}

fn int(value: &str) -> Result<i64, String> {
    let parsed = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => i64::from_str_radix(hex, 16),
        None => value.parse::<i64>(),
    };
    parsed.map_err(|_| "expected an integer".to_string())
}

/// `standard=pal,brightness=40,...` as a `CameraInput` table.
fn camera_profile(value: &str) -> Table {
    let mut t = Table::new();
    for item in value.split(',') {
        let Some((k, v)) = item.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k == "standard" {
            if ["auto", "pal", "ntsc"].contains(&v) {
                t.insert("standard".into(), Value::String(v.into()));
            }
        } else if CAMERA_LEVELS.contains(&k)
            && let Ok(n) = v.parse::<i64>()
        {
            t.insert(k.into(), Value::Integer(n));
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{self, NightMode, Settings, Standard};

    const PRESET: &str = "\
# A preset\n\
protocol   = carplay-wired\n\
ui         = true\n\
source-fps  = 60\n\
android-dpi = 135\n\
log-level = 3\n\
mfi-i2c-addr = 0x10\n\
night-mode = 2\n\
volume = -1\n\
";
    const USER: &str = "\
night-mode = 1\n\
camera-key = 88\n\
camera-fastswitch = true\n\
camera-0 = standard=pal,brightness=40,contrast=140,chroma-enhance=0\n\
audio-buffer-ms = 800 # tuned\n\
lvgl-ui = false\n\
mystery-key = 7\n\
";

    #[test]
    fn converts_both_files_in_order() {
        let import = from_fastcarplay(&[("settings.txt", PRESET), ("usersettings.txt", USER)]);
        assert_eq!(
            import.warnings,
            vec![
                "settings.txt:2: `protocol` has no LEHU setting",
                "usersettings.txt:7: `mystery-key` has no LEHU setting",
            ]
        );
        let mut merged = Settings::defaults_table();
        settings::merge(&mut merged, import.table);
        let s: Settings = Value::Table(merged).try_into().unwrap();
        assert!(!s.system.ui, "the later file wins");
        assert_eq!(s.android_auto.dpi, 135);
        assert_eq!(s.system.log_level, settings::LogLevel::Info);
        assert_eq!(s.carplay.mfi_i2c_addr, 0x10);
        assert_eq!(s.display.night_mode, NightMode::Night);
        assert_eq!(s.audio.volume, None);
        assert_eq!(s.audio.buffer_ms, 800);
        assert_eq!(s.camera.key, 88);
        assert!(s.camera.fast_switch);
        assert_eq!(s.camera.inputs.len(), 1);
        assert_eq!(s.camera.inputs[0].standard, Standard::Pal);
        assert_eq!(s.camera.inputs[0].brightness, Some(40));
        assert_eq!(s.camera.inputs[0].contrast, Some(140));
        assert_eq!(s.camera.inputs[0].saturation, None);
        assert_eq!(s.camera.inputs[0].chroma_enhance, Some(0));
    }

    #[test]
    fn only_set_keys_are_written() {
        let import = from_fastcarplay(&[("u", "volume-step = 10\n")]);
        let text = toml::to_string(&import.table).unwrap();
        assert_eq!(text.trim(), "[audio]\nvolume-step = 10");
    }

    #[test]
    fn bad_values_are_reported_not_written() {
        let import = from_fastcarplay(&[(
            "u",
            "night-mode = 9\nforce-chacha20 = maybe\nvolume-step = ten\n",
        )]);
        assert_eq!(import.warnings.len(), 3);
        assert!(import.table.is_empty());
    }

    #[test]
    fn camera_inputs_keep_their_index() {
        let import = from_fastcarplay(&[("u", "camera-2 = standard=ntsc\n")]);
        let inputs = import.table["camera"]["inputs"].as_array().unwrap();
        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs[0], Value::Table(Table::new()));
        assert_eq!(inputs[2]["standard"], Value::String("ntsc".into()));
    }
}
