// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A region: one folder of map data on the card's map partition, as
//! `scripts/map-region.py` builds it.
//!
//! ```text
//! /maps/<region>/   map.toml, tiles.pmtiles, labels.pack, roads.bin, ...
//! /maps/voice/<voice>/ the spoken prompts' clips, one folder a voice
//! ```
//!
//! The partition is written only on a PC (the card in its reader); LEHU
//! never writes it, and it is mounted read only.
//!
//! `map.toml` describes the tile set (crate::tileset) and the folder:
//! `data-version`, the layout it is in (a LEHU refuses a later one than
//! [`DATA_VERSION`]: "Map needs a newer LEHU"), `osm-date`, `preset`, and
//! under `[files]` the size and sha256 of every other file. Opening the map
//! reads only `map.toml` and the files' sizes ([`Region::health`]), which
//! finds a copy cut short; the sha256 of every file is checked once, in the
//! background, for a region new to the head unit ([`Region::verify`]), and
//! the result kept ([`Checks`]) under the user's own directory, the map
//! partition being read only.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The newest region layout this LEHU reads.
pub const DATA_VERSION: u32 = 1;

/// Where the map partition is mounted.
pub const MAPS_ROOT: &str = "/maps";
/// Its folder that is not a region: the voice sets.
pub const VOICE: &str = "voice";
/// Where map data lived before there was a map partition, used while the
/// card has none.
pub const OLD_MAP_DIR: &str = "/var/lib/lehu/maps";
pub const OLD_VOICE_DIR: &str = "/var/lib/lehu/voice/en";

/// The voice set used when none is named.
pub const DEFAULT_LANG: &str = "en";

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Manifest {
    name: Option<String>,
    data_version: Option<u32>,
    osm_date: Option<String>,
    preset: Option<String>,
    #[serde(default)]
    files: BTreeMap<String, FileSum>,
}

#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
struct FileSum {
    size: u64,
    sha256: String,
}

/// How a region stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    /// Every file there at its size, and their sha256 checked.
    Ready,
    /// Every file there at its size; the sha256 not checked yet. It is
    /// shown meanwhile.
    Unchecked,
    /// Written for a later LEHU: its data version.
    NeedsNewer(u32),
    /// A file missing, of another size, or with another sha256; or no
    /// map.toml that reads. Why, for the log.
    Damaged(String),
}

impl Health {
    /// The map can be opened on it.
    pub fn usable(&self) -> bool {
        matches!(self, Self::Ready | Self::Unchecked)
    }
}

/// A region's folder as its `map.toml` describes it.
#[derive(Clone, Debug)]
pub struct Region {
    pub dir: PathBuf,
    pub name: String,
    pub data_version: u32,
    pub osm_date: String,
    pub preset: String,
    files: Vec<(String, FileSum)>,
    /// The sha256 of map.toml itself: a region copied anew is checked
    /// anew.
    key: String,
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

impl Region {
    /// `<dir>/map.toml`. An error when there is none or it does not read.
    pub fn read(dir: &Path) -> Result<Self, String> {
        let path = dir.join("map.toml");
        let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(dir, &text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub(crate) fn parse(dir: &Path, text: &str) -> Result<Self, String> {
        let m: Manifest = toml::from_str(text).map_err(|e| e.message().to_string())?;
        let folder = dir
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let files = m
            .files
            .into_iter()
            .filter(|(name, _)| {
                // Only plain names in the folder itself.
                !name.is_empty() && !name.contains('/') && name != "." && name != ".."
            })
            .collect();
        Ok(Self {
            dir: dir.to_path_buf(),
            name: m.name.filter(|n| !n.is_empty()).unwrap_or(folder),
            data_version: m.data_version.unwrap_or(0),
            osm_date: m.osm_date.unwrap_or_default(),
            preset: m.preset.unwrap_or_default(),
            files,
            key: hex(&Sha256::digest(text.as_bytes())),
        })
    }

    /// The folder's own name (`hungary`).
    pub fn folder(&self) -> String {
        self.dir
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    }

    /// What identifies this copy of the region: the sha256 of its map.toml.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The files map.toml names, and their sizes.
    pub fn files(&self) -> impl Iterator<Item = (&str, u64)> {
        self.files.iter().map(|(n, f)| (n.as_str(), f.size))
    }

    /// The fast check, at every open: the data version, and every file's
    /// size; then what an earlier sha256 check found, when there was one.
    pub fn health(&self, checks: Option<&Checks>) -> Health {
        if self.data_version > DATA_VERSION {
            return Health::NeedsNewer(self.data_version);
        }
        for (name, sum) in &self.files {
            match fs::metadata(self.dir.join(name)) {
                Ok(m) if m.len() == sum.size => {}
                Ok(m) => {
                    return Health::Damaged(format!(
                        "{name} is {} bytes, not {}",
                        m.len(),
                        sum.size
                    ));
                }
                Err(e) => return Health::Damaged(format!("{name}: {e}")),
            }
        }
        match checks.and_then(|c| c.get(&self.key)) {
            Some(Ok(())) => Health::Ready,
            Some(Err(why)) => Health::Damaged(why.to_string()),
            // A folder from before the manifests names no files: nothing
            // to check it against.
            None if self.files.is_empty() => Health::Ready,
            None => Health::Unchecked,
        }
    }

    /// The slow check: every file's sha256 against map.toml's. Read in
    /// large pieces, each dropped from the page cache once hashed, so a
    /// gigabyte read through does not push out what the map is using.
    pub fn verify(&self) -> Result<(), String> {
        let mut buf = vec![0u8; 256 * 1024];
        for (name, sum) in &self.files {
            let path = self.dir.join(name);
            let mut file = fs::File::open(&path).map_err(|e| format!("{name}: {e}"))?;
            let mut hash = Sha256::new();
            let mut done = 0u64;
            loop {
                let n = file.read(&mut buf).map_err(|e| format!("{name}: {e}"))?;
                if n == 0 {
                    break;
                }
                hash.update(&buf[..n]);
                drop_cached(&file, done, n);
                done += n as u64;
            }
            if done != sum.size {
                return Err(format!("{name} is {done} bytes, not {}", sum.size));
            }
            if !hex(&hash.finalize()).eq_ignore_ascii_case(sum.sha256.trim()) {
                return Err(format!("{name} does not match its sha256"));
            }
        }
        Ok(())
    }
}

/// Tell the kernel the pages just read are not wanted again.
fn drop_cached(file: &fs::File, at: u64, len: usize) {
    use std::os::fd::AsRawFd;
    let (Ok(at), Ok(len)) = (libc::off_t::try_from(at), libc::off_t::try_from(len)) else {
        return;
    };
    // SAFETY: advice on a range of a file we hold open; no memory is
    // involved, and an error only means the advice was not taken.
    unsafe {
        libc::posix_fadvise(file.as_raw_fd(), at, len, libc::POSIX_FADV_DONTNEED);
    }
}

/// A region on the card, for the Settings row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    /// Its folder's name under the map partition.
    pub folder: String,
    pub dir: PathBuf,
    pub name: String,
    pub health: Health,
}

/// The folder under `root` that can hold a region: not hidden (a PC's
/// own, such as macOS's), not the voice sets.
fn region_folder(name: &str) -> bool {
    !name.starts_with('.') && name != VOICE
}

/// The regions under `root` (the map partition), by name, each with its
/// fast check. A folder without map.toml is left out.
pub fn list(root: &Path, checks: Option<&Checks>) -> Vec<Listed> {
    let mut out: Vec<Listed> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let folder = e.file_name().to_string_lossy().into_owned();
            if !region_folder(&folder) || !e.path().join("map.toml").exists() {
                return None;
            }
            let dir = e.path();
            Some(match Region::read(&dir) {
                Ok(r) => Listed {
                    health: r.health(checks),
                    name: r.name,
                    folder,
                    dir,
                },
                Err(e) => Listed {
                    name: folder.clone(),
                    health: Health::Damaged(e),
                    folder,
                    dir,
                },
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.folder.cmp(&b.folder)));
    out
}

/// The map directory to use when the settings name none: the first usable
/// region on the map partition at `root`, else the folder from before
/// there was one.
pub fn default_dir(root: &Path, checks: Option<&Checks>) -> PathBuf {
    list(root, checks)
        .into_iter()
        .find(|l| l.health.usable())
        .map_or_else(|| PathBuf::from(OLD_MAP_DIR), |l| l.dir)
}

/// The voice set to use when the settings name none (or one no longer
/// there): `<root>/voice/en` when the map partition has it (as the first
/// lehu-maps wrote it), else its first English voice, else its first
/// voice; the folder from before without one.
pub fn default_voice_dir(root: &Path) -> PathBuf {
    let dir = root.join(VOICE).join(DEFAULT_LANG);
    if dir.is_dir() {
        return dir;
    }
    let sets = voice_sets(root);
    sets.iter()
        .find(|v| v.lang == DEFAULT_LANG)
        .or(sets.first())
        .map_or_else(|| PathBuf::from(OLD_VOICE_DIR), |v| v.dir.clone())
}

/// A voice on the card: its folder, what it is called, its language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceSet {
    pub dir: PathBuf,
    /// "Anna, Hungarian", as lehu-maps names it in its map.toml; else the
    /// language's name.
    pub title: String,
    /// "hu".
    pub lang: String,
}

/// A clip every voice set has: a folder with it is one.
const VOICE_MARK: &str = "in-500.wav";

#[derive(Deserialize, Default)]
#[serde(default)]
struct VoiceManifest {
    name: String,
    title: String,
    lang: String,
}

/// The name of a language the clips speak, in English as the screens are.
pub fn language_name(code: &str) -> &str {
    match code {
        "en" => "English",
        "hu" => "Hungarian",
        "de" => "German",
        "fr" => "French",
        "es" => "Spanish",
        "it" => "Italian",
        "tr" => "Turkish",
        other => other,
    }
}

/// The voice sets under `<root>/voice` (folders with the clips, not
/// hidden), by title.
pub fn voice_sets(root: &Path) -> Vec<VoiceSet> {
    let mut out: Vec<VoiceSet> = fs::read_dir(root.join(VOICE))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let folder = e.file_name().to_string_lossy().into_owned();
            let dir = e.path();
            if folder.starts_with('.') || !dir.join(VOICE_MARK).is_file() {
                return None;
            }
            let m: VoiceManifest = fs::read_to_string(dir.join("map.toml"))
                .ok()
                .and_then(|t| toml::from_str(&t).ok())
                .unwrap_or_default();
            // The language: as written, else the first lehu-maps' name
            // ("en"), else the folder's start ("hu_HU-anna-medium").
            let lang = [m.lang.as_str(), m.name.as_str(), folder.as_str()]
                .into_iter()
                .map(|s| s.split(['_', '-']).next().unwrap_or(""))
                .find(|s| s.len() == 2)
                .unwrap_or("")
                .to_string();
            let title = if m.title.is_empty() {
                language_name(&lang).to_string()
            } else {
                m.title
            };
            Some(VoiceSet { dir, title, lang })
        })
        .collect();
    out.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.dir.cmp(&b.dir)));
    out
}

/// What the sha256 checks found, kept between starts: a line a region
/// copy, `<key> ok` or `<key> bad <why>`, in a small file of the user's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checks {
    path: PathBuf,
    found: Vec<(String, Result<(), String>)>,
}

/// Results kept, the latest last: older ones go.
const CHECKS_KEPT: usize = 32;

impl Checks {
    /// The file at `path`; empty when there is none yet.
    pub fn load(path: &Path) -> Self {
        let text = fs::read_to_string(path).unwrap_or_default();
        let found = text
            .lines()
            .filter_map(|l| {
                let (key, rest) = l.split_once(' ')?;
                let result = match rest.split_once(' ') {
                    _ if rest == "ok" => Ok(()),
                    Some(("bad", why)) => Err(why.to_string()),
                    _ => return None,
                };
                Some((key.to_string(), result))
            })
            .collect();
        Self {
            path: path.to_path_buf(),
            found,
        }
    }

    /// What the check of region copy `key` found, when it ran.
    pub fn get(&self, key: &str) -> Option<Result<(), &str>> {
        self.found
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, r)| r.as_ref().map(|_| ()).map_err(String::as_str))
    }

    /// Keep what the check of `key` found, and write the file.
    pub fn set(&mut self, key: &str, result: Result<(), String>) -> std::io::Result<()> {
        self.found.retain(|(k, _)| k != key);
        self.found.push((key.to_string(), result));
        if self.found.len() > CHECKS_KEPT {
            let extra = self.found.len() - CHECKS_KEPT;
            self.found.drain(..extra);
        }
        let text: String = self
            .found
            .iter()
            .map(|(k, r)| match r {
                Ok(()) => format!("{k} ok\n"),
                Err(why) => format!("{k} bad {}\n", why.replace('\n', " ")),
            })
            .collect();
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lehu-region-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const TILES: &str = "name = \"Test\"\nformat = \"jpg\"\ntile-size = 256\nmin-zoom = 8\n\
        max-zoom = 14\nbounds = [1000, 1000, 9000, 9000]\ncenter = [5000, 5000]\n\
        attribution = \"x\"\n";

    /// A region folder with `files` and the map.toml describing them, at
    /// data version `version`.
    fn region(dir: &Path, files: &[(&str, &[u8])], version: u32) {
        fs::create_dir_all(dir).unwrap();
        let mut toml = format!(
            "{TILES}data-version = {version}\nosm-date = \"2026-11-08\"\npreset = \"test\"\n[files]\n"
        );
        for (name, body) in files {
            fs::write(dir.join(name), body).unwrap();
            toml += &format!(
                "\"{name}\" = {{ size = {}, sha256 = \"{}\" }}\n",
                body.len(),
                hex(&Sha256::digest(body))
            );
        }
        fs::write(dir.join("map.toml"), toml).unwrap();
    }

    const FILES: &[(&str, &[u8])] = &[("roads.bin", b"roads...")];

    #[test]
    fn a_region_reads_its_manifest_and_is_checked_once_through() {
        let root = temp("checked");
        let dir = root.join("test");
        region(&dir, FILES, 1);
        let r = Region::read(&dir).unwrap();
        assert_eq!(
            (
                r.name.as_str(),
                r.osm_date.as_str(),
                r.preset.as_str(),
                r.data_version
            ),
            ("Test", "2026-11-08", "test", 1)
        );
        assert_eq!(r.files().collect::<Vec<_>>(), [("roads.bin", 8)]);
        assert_eq!(r.health(None), Health::Unchecked);
        assert_eq!(r.verify(), Ok(()));
        // The result kept, and read back from the file.
        let path = root.join("state").join("map-checks");
        let mut checks = Checks::load(&path);
        checks.set(r.key(), r.verify()).unwrap();
        let checks = Checks::load(&path);
        assert_eq!(r.health(Some(&checks)), Health::Ready);
        // A copy with another map.toml is another region to check.
        fs::write(
            dir.join("map.toml"),
            fs::read_to_string(dir.join("map.toml")).unwrap() + "# again\n",
        )
        .unwrap();
        let again = Region::read(&dir).unwrap();
        assert_ne!(again.key(), r.key());
        assert_eq!(again.health(Some(&checks)), Health::Unchecked);
    }

    #[test]
    fn a_region_cut_short_or_changed_is_damaged() {
        let dir = temp("damaged").join("test");
        region(&dir, FILES, 1);
        let r = Region::read(&dir).unwrap();
        // Same size, another byte: the sizes pass, the sha256 does not.
        fs::write(dir.join("roads.bin"), b"roads..!").unwrap();
        assert_eq!(r.health(None), Health::Unchecked);
        let why = r.verify().unwrap_err();
        assert!(why.contains("sha256"), "{why}");
        let path = dir.with_file_name("map-checks");
        let mut checks = Checks::load(&path);
        checks.set(r.key(), Err(why)).unwrap();
        assert!(matches!(r.health(Some(&checks)), Health::Damaged(w) if w.contains("sha256")));
        // Cut short: found at once.
        fs::write(dir.join("roads.bin"), b"roads").unwrap();
        assert_eq!(
            r.health(None),
            Health::Damaged("roads.bin is 5 bytes, not 8".into())
        );
        fs::remove_file(dir.join("roads.bin")).unwrap();
        assert!(matches!(r.health(None), Health::Damaged(w) if w.starts_with("roads.bin: ")));
    }

    #[test]
    fn a_region_for_a_newer_lehu_is_refused_with_why() {
        let dir = temp("newer").join("test");
        region(&dir, FILES, DATA_VERSION + 1);
        let r = Region::read(&dir).unwrap();
        assert_eq!(r.health(None), Health::NeedsNewer(DATA_VERSION + 1));
        let why = crate::TileSet::open(&dir).unwrap_err();
        assert!(why.contains("Map needs a newer LEHU"), "{why}");
        // A folder from before the manifests is read as before.
        fs::write(dir.join("map.toml"), TILES).unwrap();
        let old = Region::read(&dir).unwrap();
        assert_eq!((old.data_version, old.health(None)), (0, Health::Ready));
        assert!(crate::TileSet::open(&dir).is_ok());
    }

    #[test]
    fn the_regions_on_the_card_are_listed_by_name() {
        let root = temp("list");
        region(&root.join("b-first"), FILES, 1);
        region(&root.join("a-newer"), FILES, DATA_VERSION + 1);
        fs::write(
            root.join("a-newer").join("map.toml"),
            fs::read_to_string(root.join("a-newer").join("map.toml"))
                .unwrap()
                .replace("\"Test\"", "\"Zeta\""),
        )
        .unwrap();
        // Not regions: the voice sets, a PC's hidden folder, no map.toml.
        for d in ["voice/en", ".Trashes/x", "empty"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join(".Trashes/x/map.toml"), TILES).unwrap();
        let listed = list(&root, None);
        let seen: Vec<(&str, &str, &Health)> = listed
            .iter()
            .map(|l| (l.folder.as_str(), l.name.as_str(), &l.health))
            .collect();
        assert_eq!(
            seen,
            [
                ("b-first", "Test", &Health::Unchecked),
                ("a-newer", "Zeta", &Health::NeedsNewer(DATA_VERSION + 1)),
            ]
        );
        // The first usable one, when the settings name none; the folder
        // from before without a map partition.
        assert_eq!(default_dir(&root, None), root.join("b-first"));
        assert_eq!(
            default_dir(&root.join("missing"), None),
            PathBuf::from(OLD_MAP_DIR)
        );
        assert_eq!(default_voice_dir(&root), root.join("voice/en"));
        assert_eq!(
            default_voice_dir(&root.join("missing")),
            PathBuf::from(OLD_VOICE_DIR)
        );
    }

    /// A voice folder with a clip, and its map.toml when `toml` is given.
    fn voice(dir: &Path, toml: Option<&str>) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(VOICE_MARK), b"RIFF").unwrap();
        if let Some(t) = toml {
            fs::write(dir.join("map.toml"), t).unwrap();
        }
    }

    #[test]
    fn the_voices_on_the_card_are_listed_by_title() {
        let root = temp("voices");
        voice(
            &root.join("voice/hu_HU-anna-medium"),
            Some("name = \"hu\"\ntitle = \"Anna, Hungarian\"\nlang = \"hu\"\n"),
        );
        voice(
            &root.join("voice/en_GB-cori-high"),
            Some("name = \"en\"\ntitle = \"Cori, British English\"\nlang = \"en\"\n"),
        );
        // As the first lehu-maps wrote it: the language as its name.
        voice(&root.join("voice/old"), Some("name = \"de\"\n"));
        // No map.toml: the folder's name says the language.
        voice(&root.join("voice/tr_TR-dfki-medium"), None);
        // Not voices: no clips, hidden.
        fs::create_dir_all(root.join("voice/empty")).unwrap();
        voice(&root.join("voice/.Trash"), None);
        let seen: Vec<(String, String)> = voice_sets(&root)
            .into_iter()
            .map(|v| (v.title, v.lang))
            .collect();
        let pair = |t: &str, l: &str| (t.to_string(), l.to_string());
        assert_eq!(
            seen,
            [
                pair("Anna, Hungarian", "hu"),
                pair("Cori, British English", "en"),
                pair("German", "de"),
                pair("Turkish", "tr"),
            ]
        );
        // No voice/en: the first English one.
        assert_eq!(default_voice_dir(&root), root.join("voice/en_GB-cori-high"));
        assert!(voice_sets(&root.join("missing")).is_empty());
    }
}
