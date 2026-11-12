// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The card: its LEHU-MAPS partition found among the mounted drives, what
//! is on it, and how much room is left. Nothing here partitions or formats
//! anything.

use std::fs;
use std::path::{Path, PathBuf};

use crate::manifest::{self, Manifest};
use crate::util::{Result, io_err, plain_name};

/// The partition's label (docs/image-requirements.md).
pub const LABEL: &str = "LEHU-MAPS";
/// Its folder of voice sets, and of copies being installed.
pub const VOICE: &str = "voice";
pub const INCOMING: &str = ".incoming";

/// A place to write to: a LEHU-MAPS drive, or a folder the user chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    pub root: PathBuf,
    /// For the list: "LEHU-MAPS (/media/me/LEHU-MAPS)".
    pub title: String,
    /// The device, where known (`/dev/sdb7`, `E:`).
    pub device: String,
}

/// What is on it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Contents {
    pub regions: Vec<Item>,
    pub voices: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// Its folder under the card's root: `hungary`, `voice/en`.
    pub rel: String,
    pub name: String,
    pub osm_date: String,
    pub size: u64,
    /// Why its map.toml does not read, when it does not.
    pub problem: Option<String>,
}

/// Free and total bytes of the filesystem `path` is on.
pub fn space(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c = CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: statvfs fills the struct we give it; the path is a valid
        // C string.
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        let frag = s.f_frsize as u64;
        #[allow(clippy::unnecessary_cast)]
        return Some((s.f_bavail as u64 * frag, s.f_blocks as u64 * frag));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let (mut free, mut total, mut all_free) = (0u64, 0u64, 0u64);
        // SAFETY: a NUL-terminated path, and three u64s to fill.
        let ok =
            unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut all_free) };
        return (ok != 0).then_some((free, total));
    }
    #[allow(unreachable_code)]
    None
}

/// The mounted LEHU-MAPS drives.
pub fn detect() -> Vec<Card> {
    let mut out = Vec::new();
    #[cfg(target_os = "linux")]
    {
        let found = run::output(
            "lsblk",
            &[
                "-J",
                "-b",
                "-o",
                "NAME,LABEL,FSTYPE,MOUNTPOINT,SIZE,FSAVAIL",
            ],
        )
        .map(|j| parse_lsblk(&j))
        .unwrap_or_default();
        let found = if found.is_empty() {
            by_label_mounts()
        } else {
            found
        };
        for d in found {
            if d.label.eq_ignore_ascii_case(LABEL)
                && let Some(m) = d.mountpoint
            {
                out.push(Card {
                    title: format!("{} ({m})", d.label),
                    root: PathBuf::from(m),
                    device: format!("/dev/{}", d.name),
                });
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        // The Finder names a second drive of the same label "LEHU-MAPS 1".
        if let Ok(rd) = fs::read_dir("/Volumes") {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.eq_ignore_ascii_case(LABEL)
                    || n.to_ascii_uppercase().starts_with(&format!("{LABEL} "))
                {
                    out.push(Card {
                        title: format!("{n} (/Volumes/{n})"),
                        root: e.path(),
                        device: String::new(),
                    });
                }
            }
        }
    }
    #[cfg(windows)]
    {
        out.extend(windows_drives());
    }
    out.sort_by(|a, b| a.root.cmp(&b.root));
    out.dedup_by(|a, b| a.root == b.root);
    out
}

/// A drive lsblk lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockDev {
    pub name: String,
    pub label: String,
    pub fstype: String,
    pub mountpoint: Option<String>,
    pub size: u64,
    pub avail: Option<u64>,
}

/// `lsblk -J -b -o NAME,LABEL,FSTYPE,MOUNTPOINT,SIZE,FSAVAIL`, flattened:
/// every device and partition. Sizes come as numbers, or as strings from
/// older lsblk.
pub fn parse_lsblk(json: &str) -> Vec<BlockDev> {
    fn num(v: &serde_json::Value) -> Option<u64> {
        v.as_u64().or_else(|| v.as_str()?.trim().parse().ok())
    }
    fn s(v: &serde_json::Value) -> String {
        v.as_str().unwrap_or_default().to_string()
    }
    fn walk(v: &serde_json::Value, out: &mut Vec<BlockDev>) {
        for d in v.as_array().into_iter().flatten() {
            out.push(BlockDev {
                name: s(&d["name"]),
                label: s(&d["label"]),
                fstype: s(&d["fstype"]),
                mountpoint: d["mountpoint"]
                    .as_str()
                    .filter(|m| !m.is_empty())
                    .map(String::from),
                size: num(&d["size"]).unwrap_or(0),
                avail: num(&d["fsavail"]),
            });
            walk(&d["children"], out);
        }
    }
    let mut out = Vec::new();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json) {
        walk(&v["blockdevices"], &mut out);
    }
    out
}

/// Without lsblk: /dev/disk/by-label/LEHU-MAPS and where /proc/mounts has
/// it.
#[cfg(target_os = "linux")]
fn by_label_mounts() -> Vec<BlockDev> {
    let Ok(dev) = fs::canonicalize(Path::new("/dev/disk/by-label").join(LABEL)) else {
        return Vec::new();
    };
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    parse_mounts(&mounts, &dev.to_string_lossy())
        .into_iter()
        .map(|m| BlockDev {
            name: dev
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            label: LABEL.into(),
            fstype: String::new(),
            mountpoint: Some(m),
            size: 0,
            avail: None,
        })
        .collect()
}

/// Where /proc/mounts has `dev` mounted (`\040` in a path is a space).
pub fn parse_mounts(text: &str, dev: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            (f.next()? == dev).then(|| f.next().map(unescape_mount))?
        })
        .collect()
}

fn unescape_mount(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\134", "\\")
}

#[cfg(target_os = "linux")]
mod run {
    pub fn output(cmd: &str, args: &[&str]) -> Option<String> {
        let out = crate::proc::command(cmd).args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[cfg(windows)]
fn windows_drives() -> Vec<Card> {
    use windows_sys::Win32::Storage::FileSystem::{GetLogicalDrives, GetVolumeInformationW};
    let mut out = Vec::new();
    // SAFETY: no arguments; a bit mask of the drive letters.
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = char::from(b'A' + i as u8);
        let root: Vec<u16> = format!("{letter}:\\").encode_utf16().chain([0]).collect();
        let mut name = [0u16; 261];
        // SAFETY: a NUL-terminated root and a buffer of the length given;
        // the rest of what it can tell is not asked for.
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                name.as_mut_ptr(),
                name.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        };
        if ok == 0 {
            continue;
        }
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let label = String::from_utf16_lossy(&name[..len]);
        if label.eq_ignore_ascii_case(LABEL) {
            out.push(Card {
                title: format!("{label} ({letter}:)"),
                root: PathBuf::from(format!("{letter}:\\")),
                device: format!("{letter}:"),
            });
        }
    }
    out
}

/// A folder the user chose, as a card.
pub fn folder(path: &Path) -> Card {
    Card {
        root: path.to_path_buf(),
        title: format!("Folder {}", path.display()),
        device: String::new(),
    }
}

fn item(root: &Path, rel: String) -> Item {
    match Manifest::read(&root.join(&rel)) {
        Ok(m) => Item {
            name: if !m.title.is_empty() {
                m.title.clone()
            } else if m.name.is_empty() {
                rel.clone()
            } else {
                m.name.clone()
            },
            osm_date: m.osm_date.clone(),
            size: m.size(),
            problem: m
                .check_sizes(&root.join(&rel))
                .err()
                .map(|e| format!("damaged: {e}")),
            rel,
        },
        Err(e) => Item {
            name: rel.clone(),
            osm_date: String::new(),
            size: 0,
            problem: Some(e.to_string()),
            rel,
        },
    }
}

/// The regions (folders with a map.toml, not hidden, not `voice`) and the
/// voice sets on the card, by name.
pub fn contents(root: &Path) -> Contents {
    let dirs = |p: &Path| -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        v.sort();
        v
    };
    let regions = dirs(root)
        .into_iter()
        .filter(|n| n != VOICE && root.join(n).join(manifest::FILE).exists())
        .map(|n| item(root, n))
        .collect();
    let voices = dirs(&root.join(VOICE))
        .into_iter()
        .map(|n| item(root, format!("{VOICE}/{n}")))
        .collect();
    Contents { regions, voices }
}

/// The folder `rel` (`hungary`, `voice/en`) under `root`, when it is a
/// plain region or voice folder.
pub fn resolve(root: &Path, rel: &str) -> Option<PathBuf> {
    let parts: Vec<&str> = rel.split('/').collect();
    match parts.as_slice() {
        [r] if plain_name(r) && *r != VOICE && !r.starts_with('.') => Some(root.join(r)),
        [v, l] if *v == VOICE && plain_name(l) && !l.starts_with('.') => Some(root.join(v).join(l)),
        _ => None,
    }
}

/// Remove a region or voice set from the card.
pub fn remove(root: &Path, rel: &str) -> Result<()> {
    let dir =
        resolve(root, rel).ok_or_else(|| format!("{rel:?} is not a region or voice folder"))?;
    if !dir.join(manifest::FILE).exists() {
        return Err(format!("{} has no map.toml: not removed", dir.display()).into());
    }
    fs::remove_dir_all(&dir).map_err(|e| io_err(dir.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::tests::region;
    use crate::testutil::TempDir;

    const LSBLK: &str = r#"{
       "blockdevices": [
          {"name":"sda", "label":null, "fstype":null, "mountpoint":null, "size":31914983424, "fsavail":null,
             "children": [
                {"name":"sda1", "label":"boot", "fstype":"vfat", "mountpoint":null, "size":8388608, "fsavail":null},
                {"name":"sda4", "label":null, "fstype":null, "mountpoint":null, "size":1024, "fsavail":null,
                   "children": [
                      {"name":"sda7", "label":"LEHU-MAPS", "fstype":"exfat", "mountpoint":"/media/me/LEHU-MAPS", "size":26843545600, "fsavail":26000000000}
                   ]}
             ]},
          {"name":"nvme0n1p2", "label":"root", "fstype":"ext4", "mountpoint":"/", "size":"500000000000", "fsavail":"100"}
       ]
    }"#;

    #[test]
    fn lsblk() {
        let d = parse_lsblk(LSBLK);
        assert_eq!(d.len(), 5);
        let maps = d.iter().find(|d| d.label == LABEL).unwrap();
        assert_eq!(maps.name, "sda7");
        assert_eq!(maps.fstype, "exfat");
        assert_eq!(maps.mountpoint.as_deref(), Some("/media/me/LEHU-MAPS"));
        assert_eq!(maps.size, 26_843_545_600);
        assert_eq!(maps.avail, Some(26_000_000_000));
        let root = d.iter().find(|d| d.name == "nvme0n1p2").unwrap();
        assert_eq!((root.size, root.avail), (500_000_000_000, Some(100)));
        assert_eq!(d[0].mountpoint, None);
        assert!(parse_lsblk("not json").is_empty());
    }

    #[test]
    fn mounts() {
        let text = "/dev/sdb7 /media/me/LEHU\\040MAPS exfat rw 0 0\n/dev/sda1 / ext4 rw 0 0\n";
        assert_eq!(parse_mounts(text, "/dev/sdb7"), ["/media/me/LEHU MAPS"]);
        assert!(parse_mounts(text, "/dev/sdc1").is_empty());
    }

    #[test]
    fn contents_and_remove() {
        let t = TempDir::new("card");
        region(&t.path().join("hungary"), "Hungary");
        region(&t.path().join("luxembourg"), "Luxembourg");
        region(&t.path().join(".incoming").join("x"), "X");
        fs::create_dir_all(t.path().join("photos")).unwrap();
        let v = t.path().join("voice").join("en");
        fs::create_dir_all(&v).unwrap();
        fs::write(v.join("then.wav"), b"RIFF").unwrap();
        manifest::write_voice(&v, "en", "en_GB-cori-high", "Cori, British English, high").unwrap();
        let c = contents(t.path());
        let names: Vec<&str> = c.regions.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Hungary", "Luxembourg"]);
        assert_eq!(c.regions[0].osm_date, "2026-11-10");
        assert_eq!(c.regions[0].size, 5017);
        assert_eq!(c.regions[0].problem, None);
        assert_eq!(c.voices.len(), 1);
        assert_eq!(c.voices[0].rel, "voice/en");
        assert_eq!(c.voices[0].name, "Cori, British English, high");
        remove(t.path(), "luxembourg").unwrap();
        remove(t.path(), "voice/en").unwrap();
        assert!(remove(t.path(), "photos").is_err());
        assert!(remove(t.path(), "../etc").is_err());
        assert!(remove(t.path(), ".incoming").is_err());
        let c = contents(t.path());
        assert_eq!(c.regions.len(), 1);
        assert!(c.voices.is_empty());
        assert!(space(t.path()).is_some_and(|(free, total)| free <= total && total > 0));
    }

    #[test]
    fn damaged_region_shows() {
        let t = TempDir::new("damaged");
        region(&t.path().join("hungary"), "Hungary");
        fs::write(t.path().join("hungary/roads.bin"), b"x").unwrap();
        let c = contents(t.path());
        assert!(
            c.regions[0]
                .problem
                .as_deref()
                .unwrap()
                .starts_with("damaged")
        );
    }
}
