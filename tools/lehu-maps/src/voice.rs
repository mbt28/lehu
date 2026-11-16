// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The spoken prompts' voices: Piper voices from rhasspy/piper-voices on
//! Hugging Face, in the languages scripts/voice-clips.py has the prompts'
//! phrases in. Each goes to the card as a folder of its own, `voice/<key>`;
//! the head unit's Settings > Map > Voice picks one.

use serde::Deserialize;
use std::collections::BTreeMap;

pub const BASE: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/main";
pub const VOICES_URL: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/main/voices.json";
pub const DEFAULT: &str = "en_GB-cori-high";

/// The languages the prompts' phrases are in (scripts/voice-clips.py's
/// PHRASES), by their code, and their names.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("en", "English"),
    ("hu", "Hungarian"),
    ("de", "German"),
    ("fr", "French"),
    ("es", "Spanish"),
    ("it", "Italian"),
    ("tr", "Turkish"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Voice {
    /// `en_GB-cori-high`.
    pub key: String,
    /// "Cori, British English, high".
    pub title: String,
    /// "en": the phrases it speaks.
    pub lang: String,
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
        "es_MX" => "Mexican Spanish",
        c => language(c).map_or(c, |(_, name)| name),
    };
    format!("{n}, {lang}, {quality}")
}

/// The language of a Piper language code (`hu_HU`), when the phrases are
/// in it.
pub fn language(code: &str) -> Option<(&'static str, &'static str)> {
    let short = code.split('_').next().unwrap_or(code);
    LANGUAGES.iter().copied().find(|(c, _)| *c == short)
}

/// The voices of voices.json in the phrases' languages: the default
/// first, then by language (as LANGUAGES has them) and key.
pub fn parse(json: &str) -> Result<Vec<Voice>, String> {
    let all: BTreeMap<String, Entry> =
        serde_json::from_str(json).map_err(|e| format!("Piper's voice list does not read: {e}"))?;
    let mut out: Vec<Voice> = all
        .into_iter()
        .filter_map(|(key, e)| {
            let (lang, _) = language(&e.language.code)?;
            let pick = |suffix: &str| {
                e.files
                    .iter()
                    .find(|(p, _)| p.ends_with(suffix))
                    .map(|(p, f)| (p.clone(), f.size_bytes, f.md5_digest.clone()))
            };
            Some(Voice {
                title: title(&e.name, &e.language.code, &e.quality),
                lang: lang.into(),
                onnx: pick(".onnx")?,
                config: pick(".onnx.json")?,
                key,
            })
        })
        .collect();
    let order = |l: &str| LANGUAGES.iter().position(|(c, _)| *c == l);
    out.sort_by_key(|v| (v.key != DEFAULT, order(&v.lang), v.key.clone()));
    Ok(out)
}

/// The card's folder for `v`, under its root: `voice/en_GB-cori-high`.
pub fn folder(v: &Voice) -> String {
    format!("{}/{}", crate::card::VOICE, v.key)
}

/// The default voice when the list cannot be fetched.
pub fn fallback() -> Vec<Voice> {
    vec![Voice {
        key: DEFAULT.into(),
        title: title("cori", "en_GB", "high"),
        lang: "en".into(),
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
      "cs_CZ-jirka-medium": {"key":"cs_CZ-jirka-medium","name":"jirka","language":{"code":"cs_CZ"},"quality":"medium",
        "files":{"cs/cs_CZ/jirka/medium/cs_CZ-jirka-medium.onnx":{"size_bytes":1,"md5_digest":"a"},
                 "cs/cs_CZ/jirka/medium/cs_CZ-jirka-medium.onnx.json":{"size_bytes":1,"md5_digest":"b"}}},
      "hu_HU-anna-medium": {"key":"hu_HU-anna-medium","name":"anna","language":{"code":"hu_HU"},"quality":"medium",
        "files":{"hu/hu_HU/anna/medium/hu_HU-anna-medium.onnx":{"size_bytes":1,"md5_digest":"a"},
                 "hu/hu_HU/anna/medium/hu_HU-anna-medium.onnx.json":{"size_bytes":1,"md5_digest":"b"}}},
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
    fn the_phrases_languages_default_first() {
        let v = parse(SAMPLE).unwrap();
        let keys: Vec<&str> = v.iter().map(|v| v.key.as_str()).collect();
        // Not Czech: the prompts have no phrases in it.
        assert_eq!(
            keys,
            [
                "en_GB-cori-high",
                "en_US-lessac-medium",
                "hu_HU-anna-medium",
                "de_DE-thorsten-high"
            ]
        );
        assert_eq!(v[2].title, "Anna, Hungarian, medium");
        assert_eq!(v[2].lang, "hu");
        assert_eq!(folder(&v[2]), "voice/hu_HU-anna-medium");
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
