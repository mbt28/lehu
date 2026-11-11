// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The spoken prompts' voice: a Piper voice from rhasspy/piper-voices on
//! Hugging Face. The prompts' phrases are English (scripts/voice-clips.py),
//! so only the English voices are offered.

use serde::Deserialize;
use std::collections::BTreeMap;

pub const BASE: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/main";
pub const VOICES_URL: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/main/voices.json";
pub const DEFAULT: &str = "en_GB-cori-high";
/// The card's folder for the set, under `voice/`: the head unit asks for
/// `voice/en` (crates/lehu-map/src/region.rs).
pub const LANG: &str = "en";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Voice {
    /// `en_GB-cori-high`.
    pub key: String,
    /// "Cori, British English, high".
    pub title: String,
    /// The model and its config: (path under BASE, size, md5).
    pub onnx: (String, u64, String),
    pub config: (String, u64, String),
}

#[derive(Deserialize)]
struct Entry {
    name: String,
    language: Lang,
    quality: String,
    files: BTreeMap<String, FileInfo>,
}

#[derive(Deserialize)]
struct Lang {
    code: String,
}

#[derive(Deserialize)]
struct FileInfo {
    size_bytes: u64,
    md5_digest: String,
}

fn title(name: &str, code: &str, quality: &str) -> String {
    let mut n: String = name.replace('_', " ");
    if let Some(f) = n.get(..1) {
        n = f.to_uppercase() + &n[1..];
    }
    let lang = match code {
        "en_GB" => "British English",
        "en_US" => "American English",
        c => c,
    };
    format!("{n}, {lang}, {quality}")
}

/// The English voices of voices.json, the default first, then by key.
pub fn parse(json: &str) -> Result<Vec<Voice>, String> {
    let all: BTreeMap<String, Entry> =
        serde_json::from_str(json).map_err(|e| format!("Piper's voice list does not read: {e}"))?;
    let mut out: Vec<Voice> = all
        .into_iter()
        .filter(|(_, e)| matches!(e.language.code.as_str(), "en_GB" | "en_US"))
        .filter_map(|(key, e)| {
            let pick = |suffix: &str| {
                e.files
                    .iter()
                    .find(|(p, _)| p.ends_with(suffix))
                    .map(|(p, f)| (p.clone(), f.size_bytes, f.md5_digest.clone()))
            };
            Some(Voice {
                title: title(&e.name, &e.language.code, &e.quality),
                onnx: pick(".onnx")?,
                config: pick(".onnx.json")?,
                key,
            })
        })
        .collect();
    out.sort_by_key(|v| (v.key != DEFAULT, v.key.clone()));
    Ok(out)
}

/// The default voice when the list cannot be fetched.
pub fn fallback() -> Vec<Voice> {
    vec![Voice {
        key: DEFAULT.into(),
        title: title("cori", "en_GB", "high"),
        onnx: (
            "en/en_GB/cori/high/en_GB-cori-high.onnx".into(),
            114_219_352,
            "3474a80133d9a03e6870d2ac42c18806".into(),
        ),
        config: (
            "en/en_GB/cori/high/en_GB-cori-high.onnx.json".into(),
            4963,
            "0f7d42e77a99193006aa34a34442f5e0".into(),
        ),
    }]
}

pub fn url(path: &str) -> String {
    format!("{BASE}/{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "de_DE-thorsten-high": {"key":"de_DE-thorsten-high","name":"thorsten","language":{"code":"de_DE"},"quality":"high",
        "files":{"de/de_DE/thorsten/high/de_DE-thorsten-high.onnx":{"size_bytes":1,"md5_digest":"a"},
                 "de/de_DE/thorsten/high/de_DE-thorsten-high.onnx.json":{"size_bytes":1,"md5_digest":"b"}}},
      "en_US-lessac-medium": {"key":"en_US-lessac-medium","name":"lessac","language":{"code":"en_US"},"quality":"medium",
        "files":{"en/en_US/lessac/medium/en_US-lessac-medium.onnx":{"size_bytes":63201294,"md5_digest":"2fc6"},
                 "en/en_US/lessac/medium/en_US-lessac-medium.onnx.json":{"size_bytes":4885,"md5_digest":"c0de"},
                 "en/en_US/lessac/medium/MODEL_CARD":{"size_bytes":10,"md5_digest":"d"}}},
      "en_GB-cori-high": {"key":"en_GB-cori-high","name":"cori","language":{"code":"en_GB"},"quality":"high",
        "files":{"en/en_GB/cori/high/en_GB-cori-high.onnx":{"size_bytes":114219352,"md5_digest":"3474a80133d9a03e6870d2ac42c18806"},
                 "en/en_GB/cori/high/en_GB-cori-high.onnx.json":{"size_bytes":4963,"md5_digest":"0f7d42e77a99193006aa34a34442f5e0"}}}
    }"#;

    #[test]
    fn english_only_default_first() {
        let v = parse(SAMPLE).unwrap();
        let keys: Vec<&str> = v.iter().map(|v| v.key.as_str()).collect();
        assert_eq!(keys, ["en_GB-cori-high", "en_US-lessac-medium"]);
        assert_eq!(v[0], fallback()[0]);
        assert_eq!(v[1].title, "Lessac, American English, medium");
        assert_eq!(
            v[1].config.0,
            "en/en_US/lessac/medium/en_US-lessac-medium.onnx.json"
        );
        assert_eq!(
            url(&v[0].onnx.0),
            "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_GB/cori/high/en_GB-cori-high.onnx"
        );
    }
}
