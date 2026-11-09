// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! What LEHU is doing, as a small JSON file for another process on the
//! board (a settings or update service) to show, with no protocol between
//! them. Ported from FastCarPlay's status file, at /run/lehu/status.json.
//!
//! It lives on /run, a tmpfs: the file is rewritten for as long as LEHU
//! runs, and on the SD card that heartbeat would be a stream of flash
//! writes for nothing. A reader takes a stale mtime or a dead pid as "LEHU
//! is not running": no signals, no socket, and it tells the truth in the
//! one case that matters, LEHU having crashed or hung.

use std::fs;
use std::io;
use std::path::PathBuf;

use lehu_core::clock;

/// Rewritten at least this often, so the mtime says the program lives.
const HEARTBEAT_MS: u64 = 5000;

/// What the file carries; written when any of it changes, or on the
/// heartbeat.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fields {
    /// FastCarPlay's states: 1 no phone, 2 connecting, 4 online (a phone
    /// on a link, no video yet), 5 connected (the phone's video).
    pub state: i32,
    pub status: String,
    pub phone_name: String,
    /// "carplay", "android-auto" or empty.
    pub protocol: String,
    /// "usb", "wifi" or empty.
    pub link: String,
    pub video_focused: bool,
}

pub struct StatusFile {
    path: PathBuf,
    last: Option<Fields>,
    written_at: u64,
    /// A path that cannot be written is not tried again.
    disabled: bool,
}

impl StatusFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut disabled = false;
        if let Some(dir) = path.parent()
            && let Err(e) = fs::create_dir_all(dir)
        {
            log::info!("status: no {} ({e}); no status file", dir.display());
            disabled = true;
        }
        Self {
            path,
            last: None,
            written_at: 0,
            disabled,
        }
    }

    /// Write when something a reader sees changed, or the heartbeat is due.
    pub fn update(&mut self, fields: &Fields) {
        if self.disabled {
            return;
        }
        let now = clock::now_ms();
        let changed = self.last.as_ref() != Some(fields);
        if !changed && now.saturating_sub(self.written_at) < HEARTBEAT_MS {
            return;
        }
        match self.write(fields) {
            Ok(()) => {
                self.last = Some(fields.clone());
                self.written_at = now;
            }
            Err(e) => {
                log::warn!(
                    "status: cannot write {} ({e}); status file disabled",
                    self.path.display()
                );
                self.disabled = true;
            }
        }
    }

    /// Through a temporary file and a rename, so a reader never sees half
    /// a document. No fsync: on a tmpfs, and durability across a power cut
    /// is exactly what a liveness file must not have.
    fn write(&self, f: &Fields) -> io::Result<()> {
        let body = format!(
            "{{\n  \"pid\": {},\n  \"app\": \"lehu\",\n  \"app_version\": {},\n  \"state\": {},\n  \"status\": {},\n  \"phone_name\": {},\n  \"protocol\": {},\n  \"link\": {},\n  \"video_focused\": {}\n}}\n",
            std::process::id(),
            quoted(env!("CARGO_PKG_VERSION")),
            f.state,
            quoted(&f.status),
            quoted(&f.phone_name),
            quoted(&f.protocol),
            quoted(&f.link),
            f.video_focused
        );
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, body)?;
        fs::rename(&tmp, &self.path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
    }
}

/// A JSON string: quotes, backslashes and control characters escaped.
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_escaped() {
        assert_eq!(quoted("a\"b\\c\n\u{1}"), "\"a\\\"b\\\\c\\n\\u0001\"");
        assert_eq!(quoted("Alex\u{2019}s iPhone"), "\"Alex\u{2019}s iPhone\"");
    }

    #[test]
    fn written_on_change_and_parsed_back() {
        let dir = std::env::temp_dir().join(format!("lehu-status-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("status.json");
        let mut s = StatusFile::new(&path);
        let mut f = Fields {
            state: 5,
            status: "CarPlay: Phone".into(),
            phone_name: "Phone".into(),
            protocol: "carplay".into(),
            link: "wifi".into(),
            video_focused: true,
        };
        s.update(&f);
        let text = fs::read_to_string(&path).unwrap();
        let v: toml::Value = serde_json_lite(&text);
        assert_eq!(v.get("state").and_then(toml::Value::as_integer), Some(5));
        assert_eq!(v.get("link").and_then(toml::Value::as_str), Some("wifi"));
        assert_eq!(
            v.get("video_focused").and_then(toml::Value::as_bool),
            Some(true)
        );
        // Unchanged within the heartbeat: not rewritten.
        fs::write(&path, "x").unwrap();
        s.update(&f);
        assert_eq!(fs::read_to_string(&path).unwrap(), "x");
        f.state = 1;
        s.update(&f);
        assert!(fs::read_to_string(&path).unwrap().contains("\"state\": 1"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// The document is flat JSON of strings, integers and booleans, which
    /// TOML's inline-table syntax reads once the colons become equals.
    fn serde_json_lite(text: &str) -> toml::Value {
        let mut t = toml::Table::new();
        for line in text.lines() {
            let line = line.trim().trim_end_matches(',');
            let Some((k, v)) = line.split_once(": ") else {
                continue;
            };
            let k = k.trim_matches('"');
            let v: toml::Value = toml::from_str::<toml::Table>(&format!("v = {v}"))
                .ok()
                .and_then(|mut m| m.remove("v"))
                .unwrap_or(toml::Value::String(String::new()));
            t.insert(k.into(), v);
        }
        toml::Value::Table(t)
    }
}
