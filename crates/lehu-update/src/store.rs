// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The folder an update is received into (`/var/cache/lehu-update`, on the
//! SD card, never the RAM of `/tmp`): its names, the part being received,
//! what is left over, the space there is.
//!
//! The installer unpacks a checked tar beside itself, as root, and removes
//! the tar; the folder it leaves has the tar's name without `.tar`, and the
//! `.tar` name keeps working for `apply`.

use std::ffi::{CString, OsStr};
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// The largest update taken. A LEHU OS update is tens of MB.
pub const MAX_SIZE: u64 = 512 << 20;
/// Kept free on the card beyond what is received.
pub const SPARE: u64 = 16 << 20;
/// A file name's longest.
const NAME_MAX: usize = 160;

/// A name the phone may give an update: `[A-Za-z0-9._+-]`, ending `.tar`,
/// not hidden. No `/`, so it is always a file directly in the folder.
pub fn valid_name(name: &str) -> bool {
    name.len() > 4
        && name.len() <= NAME_MAX
        && name.ends_with(".tar")
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

/// Where `file` is received until it is complete.
pub fn part_path(dir: &Path, file: &str) -> PathBuf {
    dir.join(format!("{file}.part"))
}

/// How much of `file` is there already; 0 for none.
pub fn part_len(dir: &Path, file: &str) -> u64 {
    fs::metadata(part_path(dir, file)).map_or(0, |m| m.len())
}

/// The part being received, the newest if there were several: its file
/// name and length.
pub fn current_part(dir: &Path) -> Option<(String, u64)> {
    newest(dir, |name, meta| {
        let file = name.strip_suffix(".part")?;
        (meta.is_file() && valid_name(file)).then(|| (file.to_string(), meta.len()))
    })
}

/// An update received earlier and not installed: a tar, or the folder a
/// check unpacked it to; as the `.tar` path the installer takes.
pub fn leftover(dir: &Path) -> Option<PathBuf> {
    newest(dir, |name, meta| {
        let tar = if name.ends_with(".new") {
            // The installer's, half unpacked.
            return None;
        } else if meta.is_dir() {
            format!("{name}.tar")
        } else if meta.is_file() {
            name.to_string()
        } else {
            return None;
        };
        valid_name(&tar).then(|| dir.join(tar))
    })
}

fn newest<T>(dir: &Path, mut pick: impl FnMut(&str, &fs::Metadata) -> Option<T>) -> Option<T> {
    let mut best: Option<(std::time::SystemTime, T)> = None;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        if let Some(found) = pick(name, &meta) {
            let at = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(t, _)| at > *t) {
                best = Some((at, found));
            }
        }
    }
    best.map(|(_, found)| found)
}

/// Everything in the folder but `keep` goes: older updates, parts of
/// other ones. What root unpacked may not be removable by LEHU; that stays
/// (the installer replaces it when the same update comes again).
pub fn remove_others(dir: &Path, keep: &[&str]) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if keep.iter().any(|k| OsStr::new(k) == name) {
            continue;
        }
        let path = entry.path();
        let removed = match entry.file_type() {
            Ok(t) if t.is_dir() => fs::remove_dir_all(&path),
            Ok(_) => fs::remove_file(&path),
            Err(e) => Err(e),
        };
        match removed {
            Ok(()) => log::info!("update: removed {}", path.display()),
            Err(e) => log::debug!("update: {} stays: {e}", path.display()),
        }
    }
}

/// The bytes an unprivileged writer may still use there.
pub fn free_bytes(dir: &Path) -> io::Result<u64> {
    let path = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: a zeroed statvfs filled in by the call, from a NUL-terminated
    // path that lives across it.
    let st = unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path.as_ptr(), &mut st) != 0 {
            return Err(io::Error::last_os_error());
        }
        st
    };
    // Their width depends on the target.
    Ok((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

/// The SHA-256 of a file, read in small pieces.
pub fn sha256_file(path: &Path) -> io::Result<[u8; 32]> {
    let mut f = fs::File::open(path)?;
    let mut hasher = openssl::sha::Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(hasher.finish());
        }
        hasher.update(&buf[..n]);
    }
}

/// 64 hex digits as the 32 bytes they spell, either case.
pub fn parse_sha256(hex: &str) -> Option<[u8; 32]> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
        let digit = |b: u8| (b as char).to_digit(16);
        out[i] = (digit(pair[0])? * 16 + digit(pair[1])?) as u8;
    }
    Some(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh folder for one test, on the host.
    pub(crate) fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lehu-update-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn names_are_plain_tar_files() {
        assert!(valid_name("lehu-os-22.0.1-20261108095815-from-6888804.tar"));
        assert!(valid_name("a+b_c.d-e.tar"));
        for bad in [
            ".tar",
            "x.tar.gz",
            "../x.tar",
            "a/b.tar",
            ".hidden.tar",
            "sp ace.tar",
            "x.tar\0",
            "ü.tar",
            "x.TAR",
            "",
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert!(!valid_name(&format!("{}.tar", "a".repeat(NAME_MAX))));
    }

    #[test]
    fn parts_leftovers_and_cleaning() {
        let d = dir("store");
        assert_eq!(current_part(&d), None);
        assert_eq!(leftover(&d), None);
        fs::write(part_path(&d, "a.tar"), b"12345").unwrap();
        fs::write(d.join("junk.txt"), b"x").unwrap();
        assert_eq!(part_len(&d, "a.tar"), 5);
        assert_eq!(part_len(&d, "b.tar"), 0);
        assert_eq!(current_part(&d), Some(("a.tar".to_string(), 5)));
        // A part is not an update yet.
        assert_eq!(leftover(&d), None);
        fs::create_dir(d.join("old")).unwrap();
        assert_eq!(leftover(&d), Some(d.join("old.tar")));
        // The installer's own folders are not updates.
        fs::create_dir(d.join(".tmp")).unwrap();
        fs::create_dir(d.join("old.new")).unwrap();
        fs::write(d.join("old.new").join("f"), b"x").unwrap();
        let far = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        fs::File::open(d.join("old.new"))
            .unwrap()
            .set_modified(far)
            .unwrap();
        assert_eq!(leftover(&d), Some(d.join("old.tar")));
        remove_others(&d, &["a.tar.part"]);
        let left: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["a.tar.part"]);
        assert!(free_bytes(&d).unwrap() > 0);
    }

    #[test]
    fn sha256_of_a_file_and_its_hex() {
        let d = dir("sha");
        let f = d.join("x");
        fs::write(&f, b"abc").unwrap();
        let want = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(sha256_file(&f).unwrap(), parse_sha256(want).unwrap());
        assert_eq!(
            parse_sha256(&want.to_uppercase()),
            parse_sha256(want),
            "either case"
        );
        assert_eq!(parse_sha256(&want[..62]), None);
        assert_eq!(parse_sha256(&format!("{}zz", &want[..62])), None);
    }
}
