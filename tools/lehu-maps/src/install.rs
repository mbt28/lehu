// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Writing a region or voice set to the card: copied into
//! `<card>/.incoming/`, flushed, read back and every file's sha256 checked
//! against its map.toml, then renamed into its place. An older copy there
//! is replaced only once the new one has been checked.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::card::{INCOMING, resolve};
use crate::manifest::{self, Manifest};
use crate::util::{Cancel, Error, Result, io_err};

/// How an install is going.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    /// Bytes copied of the total.
    Copy(u64, u64),
    /// Bytes read back and checked of the total.
    Verify(u64, u64),
}

/// The staging folder for `rel` (`voice/en` stages as `voice-en`).
pub fn staged(root: &Path, rel: &str) -> PathBuf {
    root.join(INCOMING).join(rel.replace('/', "-"))
}

/// `src` (a folder with its map.toml) onto the card at `root` as `rel`.
pub fn install(
    src: &Path,
    root: &Path,
    rel: &str,
    cancel: &Cancel,
    progress: &mut dyn FnMut(Step),
) -> Result<PathBuf> {
    let target =
        resolve(root, rel).ok_or_else(|| format!("{rel:?} is not a folder name for the card"))?;
    let m = Manifest::read(src)?;
    m.check_sizes(src).map_err(|e| {
        Error::Msg(format!(
            "the built copy in {} is not whole: {e}",
            src.display()
        ))
    })?;
    let stage = staged(root, rel);
    let r = copy_and_check(src, &stage, &m, cancel, progress)
        .and_then(|()| put_in_place(&stage, &target, root));
    if r.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    // Empty, it goes.
    let _ = fs::remove_dir(root.join(INCOMING));
    r.map(|()| target)
}

fn copy_and_check(
    src: &Path,
    stage: &Path,
    m: &Manifest,
    cancel: &Cancel,
    progress: &mut dyn FnMut(Step),
) -> Result<()> {
    let _ = fs::remove_dir_all(stage);
    fs::create_dir_all(stage).map_err(|e| io_err(stage.display(), e))?;
    let total = m.size();
    let mut done = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    progress(Step::Copy(0, total));
    // The files first, map.toml last: a folder that has its map.toml has
    // everything.
    let names = m.files.keys().map(String::as_str).chain([manifest::FILE]);
    for name in names {
        let from = src.join(name);
        let to = stage.join(name);
        let mut r = File::open(&from).map_err(|e| io_err(from.display(), e))?;
        let mut w = File::create(&to).map_err(|e| io_err(to.display(), e))?;
        loop {
            cancel.check()?;
            let n = r.read(&mut buf).map_err(|e| io_err(from.display(), e))?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n])
                .map_err(|e| io_err(format!("writing {} to the card", to.display()), e))?;
            if name != manifest::FILE {
                done += n as u64;
                progress(Step::Copy(done, total));
            }
        }
        w.sync_all()
            .map_err(|e| io_err(format!("flushing {}", to.display()), e))?;
        drop_cache(&w);
    }
    sync_dir(stage);
    // Read back from the card: the manifest copied there, and every file
    // against it.
    let copied = Manifest::read(stage)?;
    if copied != *m {
        return Err("map.toml did not copy whole".into());
    }
    progress(Step::Verify(0, total));
    copied
        .verify(stage, cancel, &mut |b| progress(Step::Verify(b, total)))
        .map_err(|e| match e {
            Error::Cancelled => e,
            Error::Msg(m) => Error::Msg(format!("the copy on the card failed its check: {m}")),
        })
}

/// Rename the checked copy into place, an older one aside first and
/// removed after.
fn put_in_place(stage: &Path, target: &Path, root: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| io_err(parent.display(), e))?;
    }
    let name = target
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let aside = root
        .join(INCOMING)
        .join(format!(".old-{name}-{}", std::process::id()));
    let had = target.exists();
    if had {
        let _ = fs::remove_dir_all(&aside);
        fs::rename(target, &aside)
            .map_err(|e| io_err(format!("moving the old {} aside", target.display()), e))?;
    }
    if let Err(e) = fs::rename(stage, target) {
        if had {
            let _ = fs::rename(&aside, target);
        }
        return Err(io_err(format!("renaming into {}", target.display()), e));
    }
    if had {
        let _ = fs::remove_dir_all(&aside);
    }
    if let Some(parent) = target.parent() {
        sync_dir(parent);
    }
    Ok(())
}

/// The pages just written are not kept, so that the check reads the card
/// rather than memory (Linux; elsewhere the check may read the cache).
fn drop_cache(_f: &File) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: advice on a file we hold open; an error only means it was
        // not taken.
        unsafe {
            libc::posix_fadvise(_f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
        }
    }
}

fn sync_dir(_dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = File::open(_dir) {
        let _ = d.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::tests::region;
    use crate::testutil::TempDir;

    #[test]
    fn new_region() {
        let t = TempDir::new("install");
        let src = t.path().join("build");
        let card = t.path().join("card");
        fs::create_dir_all(&card).unwrap();
        region(&src, "Testland");
        let mut steps = Vec::new();
        let dir = install(&src, &card, "testland", &Cancel::new(), &mut |s| {
            steps.push(s)
        })
        .unwrap();
        assert_eq!(dir, card.join("testland"));
        assert_eq!(
            fs::read(dir.join("roads.bin")).unwrap(),
            b"roads roads roads"
        );
        assert!(dir.join("map.toml").exists());
        assert!(!card.join(INCOMING).exists());
        assert_eq!(steps.first(), Some(&Step::Copy(0, 5017)));
        assert_eq!(steps.last(), Some(&Step::Verify(5017, 5017)));
    }

    #[test]
    fn replaces_an_older_copy() {
        let t = TempDir::new("replace");
        let src = t.path().join("build");
        let card = t.path().join("card");
        region(&src, "Testland v2");
        region(&card.join("testland"), "Testland v1");
        fs::write(card.join("testland/old-only.bin"), b"x").unwrap();
        install(&src, &card, "testland", &Cancel::new(), &mut |_| {}).unwrap();
        assert_eq!(
            Manifest::read(&card.join("testland")).unwrap().name,
            "Testland v2"
        );
        assert!(!card.join("testland/old-only.bin").exists());
        assert!(!card.join(INCOMING).exists());
    }

    #[test]
    fn a_bad_build_leaves_the_old_copy() {
        let t = TempDir::new("bad");
        let src = t.path().join("build");
        let card = t.path().join("card");
        region(&src, "Testland v2");
        // The same size, other bytes: only the sha256 finds it.
        fs::write(src.join("roads.bin"), b"roads roads roadz").unwrap();
        region(&card.join("testland"), "Testland v1");
        let e = install(&src, &card, "testland", &Cancel::new(), &mut |_| {}).unwrap_err();
        assert!(e.to_string().contains("failed its check"), "{e}");
        assert_eq!(
            Manifest::read(&card.join("testland")).unwrap().name,
            "Testland v1"
        );
        assert!(!card.join(INCOMING).exists());
    }

    #[test]
    fn cancelled_and_voice() {
        let t = TempDir::new("cancel");
        let src = t.path().join("build");
        let card = t.path().join("card");
        fs::create_dir_all(&card).unwrap();
        region(&src, "Testland");
        let c = Cancel::new();
        c.cancel();
        assert_eq!(
            install(&src, &card, "testland", &c, &mut |_| {}).unwrap_err(),
            Error::Cancelled
        );
        assert!(!card.join("testland").exists() && !card.join(INCOMING).exists());
        let dir = install(&src, &card, "voice/en", &Cancel::new(), &mut |_| {}).unwrap();
        assert_eq!(dir, card.join("voice").join("en"));
        assert!(install(&src, &card, "../x", &Cancel::new(), &mut |_| {}).is_err());
    }

    #[test]
    fn a_real_region() {
        // A region map-region.py built, when LEHU_MAPS_TEST_REGION names
        // one (map data is never in the repository).
        let Some(src) = std::env::var_os("LEHU_MAPS_TEST_REGION").map(PathBuf::from) else {
            return;
        };
        let t = TempDir::new("real");
        let dir = install(&src, t.path(), "region", &Cancel::new(), &mut |_| {}).unwrap();
        let m = Manifest::read(&dir).unwrap();
        assert!(m.files.contains_key("tiles.pmtiles"));
        assert_eq!(m, Manifest::read(&src).unwrap());
    }
}
