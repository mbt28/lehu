// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A folder's `map.toml`, as scripts/map-region.py writes it for a region
//! and for a voice set: its name, data version, OSM date, preset, and the
//! size and sha256 of every other file, under `[files]`. The head unit
//! reads the same (crates/lehu-map/src/region.rs).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::util::{Cancel, Error, Result, hex, io_err, plain_name};

pub const FILE: &str = "map.toml";

/// The region layout the head unit reads, and this tool writes for a voice
/// set.
pub const DATA_VERSION: u32 = 1;

#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FileSum {
    pub size: u64,
    pub sha256: String,
}

#[derive(Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct Manifest {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub data_version: u32,
    #[serde(default)]
    pub osm_date: String,
    #[serde(default)]
    pub preset: String,
    /// A voice set's: "Anna, Hungarian, medium", "hu", the Piper voice.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub lang: String,
    #[serde(default)]
    pub voice: String,
    #[serde(default)]
    pub files: BTreeMap<String, FileSum>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self> {
        let m: Self = toml::from_str(text).map_err(|e| Error::Msg(e.message().to_string()))?;
        if let Some(bad) = m.files.keys().find(|n| !plain_name(n)) {
            return Err(format!("names a file outside its folder: {bad:?}").into());
        }
        Ok(m)
    }

    /// `<dir>/map.toml`.
    pub fn read(dir: &Path) -> Result<Self> {
        let path = dir.join(FILE);
        let text = fs::read_to_string(&path).map_err(|e| io_err(path.display(), e))?;
        Self::parse(&text).map_err(|e| Error::Msg(format!("{}: {e}", path.display())))
    }

    /// The files' bytes, map.toml's own not counted.
    pub fn size(&self) -> u64 {
        self.files.values().map(|f| f.size).sum()
    }

    /// Every file's size, quickly: what a copy cut short fails.
    pub fn check_sizes(&self, dir: &Path) -> Result<()> {
        for (name, sum) in &self.files {
            let path = dir.join(name);
            let len = fs::metadata(&path).map_err(|e| io_err(name, e))?.len();
            if len != sum.size {
                return Err(format!("{name} is {len} bytes, not {}", sum.size).into());
            }
        }
        Ok(())
    }

    /// Every file read whole and its sha256 matched to map.toml's.
    /// `progress(bytes)` as it reads.
    pub fn verify(&self, dir: &Path, cancel: &Cancel, progress: &mut dyn FnMut(u64)) -> Result<()> {
        self.check_sizes(dir)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0;
        for (name, sum) in &self.files {
            let path = dir.join(name);
            let mut f = File::open(&path).map_err(|e| io_err(name, e))?;
            let mut h = Sha256::new();
            let mut done = 0u64;
            loop {
                cancel.check()?;
                let n = f.read(&mut buf).map_err(|e| io_err(name, e))?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                done += n as u64;
                total += n as u64;
                progress(total);
            }
            if done != sum.size {
                return Err(format!("{name} is {done} bytes, not {}", sum.size).into());
            }
            if !hex(&h.finalize()).eq_ignore_ascii_case(sum.sha256.trim()) {
                return Err(
                    format!("{name} does not match its sha256: the copy is damaged").into(),
                );
            }
        }
        Ok(())
    }
}

/// The sha256 of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).map_err(|e| io_err(path.display(), e))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| io_err(path.display(), e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `<dir>/map.toml` for a voice set: the language as the name (as the
/// first head units read it), the data version, the title the head unit's
/// Voice row shows, the language, the Piper voice, and each clip's size
/// and sha256.
pub fn write_voice(dir: &Path, lang: &str, voice: &str, title: &str) -> Result<Manifest> {
    let mut clips: Vec<String> = fs::read_dir(dir)
        .map_err(|e| io_err(dir.display(), e))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".wav"))
        .collect();
    clips.sort();
    if clips.is_empty() {
        return Err(format!("{}: no clips", dir.display()).into());
    }
    let mut text = format!(
        "# Written by lehu-maps: a set of spoken prompts, Piper voice {voice}.\n\
         name = {}\ndata-version = {DATA_VERSION}\ntitle = {}\nlang = {}\nvoice = {}\n\n[files]\n",
        toml_str(lang),
        toml_str(title),
        toml_str(lang),
        toml_str(voice)
    );
    for c in &clips {
        let p = dir.join(c);
        let size = fs::metadata(&p).map_err(|e| io_err(p.display(), e))?.len();
        text += &format!(
            "{} = {{ size = {size}, sha256 = {} }}\n",
            toml_str(c),
            toml_str(&sha256_file(&p)?)
        );
    }
    let tmp = dir.join("map.toml.tmp");
    fs::write(&tmp, &text).map_err(|e| io_err(tmp.display(), e))?;
    fs::rename(&tmp, dir.join(FILE)).map_err(|e| io_err(dir.display(), e))?;
    Manifest::parse(&text)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// A tiny region folder: two files and their map.toml.
    pub fn region(dir: &Path, name: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("roads.bin"), b"roads roads roads").unwrap();
        fs::write(dir.join("tiles.pmtiles"), vec![7u8; 5000]).unwrap();
        let mut text = format!(
            "name = \"{name}\"\ndata-version = 1\nosm-date = \"2026-11-10\"\npreset = \"auto\"\n[files]\n"
        );
        for f in ["roads.bin", "tiles.pmtiles"] {
            let p = dir.join(f);
            text += &format!(
                "\"{f}\" = {{ size = {}, sha256 = \"{}\" }}\n",
                fs::metadata(&p).unwrap().len(),
                sha256_file(&p).unwrap()
            );
        }
        fs::write(dir.join(FILE), text).unwrap();
    }

    #[test]
    fn good_region() {
        let t = TempDir::new("good");
        region(t.path(), "Testland");
        let m = Manifest::read(t.path()).unwrap();
        assert_eq!(m.name, "Testland");
        assert_eq!(m.osm_date, "2026-11-10");
        assert_eq!(m.size(), 5017);
        let mut seen = 0;
        m.verify(t.path(), &Cancel::new(), &mut |b| seen = b)
            .unwrap();
        assert_eq!(seen, 5017);
    }

    #[test]
    fn size_mismatch() {
        let t = TempDir::new("size");
        region(t.path(), "Testland");
        fs::write(t.path().join("roads.bin"), b"short").unwrap();
        let m = Manifest::read(t.path()).unwrap();
        let e = m.verify(t.path(), &Cancel::new(), &mut |_| {}).unwrap_err();
        assert_eq!(e.to_string(), "roads.bin is 5 bytes, not 17");
    }

    #[test]
    fn sha_mismatch() {
        let t = TempDir::new("sha");
        region(t.path(), "Testland");
        fs::write(t.path().join("roads.bin"), b"roads roads roadz").unwrap();
        let m = Manifest::read(t.path()).unwrap();
        let e = m.verify(t.path(), &Cancel::new(), &mut |_| {}).unwrap_err();
        assert!(
            e.to_string()
                .contains("roads.bin does not match its sha256"),
            "{e}"
        );
    }

    #[test]
    fn missing_file_and_bad_names() {
        let t = TempDir::new("missing");
        region(t.path(), "Testland");
        fs::remove_file(t.path().join("tiles.pmtiles")).unwrap();
        let m = Manifest::read(t.path()).unwrap();
        assert!(m.check_sizes(t.path()).is_err());
        assert!(Manifest::parse("[files]\n\"../x\" = { size = 1, sha256 = \"\" }\n").is_err());
    }

    #[test]
    fn voice_manifest() {
        let t = TempDir::new("voice");
        fs::write(t.path().join("turn-left.wav"), b"RIFF....").unwrap();
        fs::write(t.path().join("then.wav"), b"RIFF..").unwrap();
        fs::write(t.path().join("notes.txt"), b"x").unwrap();
        let m = write_voice(
            t.path(),
            "hu",
            "hu_HU-anna-medium",
            "Anna, Hungarian, medium",
        )
        .unwrap();
        assert_eq!(m.name, "hu");
        assert_eq!(
            (m.title.as_str(), m.lang.as_str(), m.voice.as_str()),
            ("Anna, Hungarian, medium", "hu", "hu_HU-anna-medium")
        );
        assert_eq!(m.files.len(), 2);
        let again = Manifest::read(t.path()).unwrap();
        assert_eq!(again, m);
        again.verify(t.path(), &Cancel::new(), &mut |_| {}).unwrap();
    }
}
