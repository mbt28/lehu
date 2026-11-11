// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The work behind "Write to card", step by step: the extract downloaded
//! (or found in the cache), the Python environment, the size estimate and
//! the card's room, the region built with scripts/map-region.py (or found
//! built), the voice made, both written to the card and checked.

use std::fs;
use std::path::{Path, PathBuf};

use crate::card;
use crate::geofabrik::{self, Area};
use crate::http::{self, Http};
use crate::install::{self, Step};
use crate::manifest::{self, Manifest};
use crate::proc::{self, Stream};
use crate::progress::{self, Line};
use crate::python;
use crate::util::{Cancel, Error, Result, human_size, io_err};
use crate::voice::{self, Voice};

/// What the worker tells the window.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A new step, for the line above the bar.
    Stage(String),
    /// How far, 0..1, or below 0 when not known; and a detail ("12 of 48 MB").
    Progress(f32, String),
    /// A line for the log.
    Log(String),
}

pub type Report<'a> = &'a mut dyn FnMut(Event);

/// What the work needs.
#[derive(Clone)]
pub struct Ctx {
    pub cache: PathBuf,
    /// The folder with map-region.py and voice-clips.py.
    pub scripts: Option<PathBuf>,
    pub http: Http,
    pub cancel: Cancel,
    pub workers: usize,
}

/// An extract in the cache.
#[derive(Clone, Debug)]
pub struct Extract {
    pub path: PathBuf,
    /// Its date, from Geofabrik's dated file name.
    pub date: Option<String>,
    pub md5: String,
}

/// The folder holding map-region.py: `explicit` (the folder or the
/// repository), else `scripts/` of a LEHU checkout above the current
/// folder or the program.
pub fn find_scripts(explicit: Option<&Path>) -> Option<PathBuf> {
    let ok = |d: &Path| d.join("map-region.py").is_file();
    if let Some(p) = explicit {
        return [p.to_path_buf(), p.join("scripts")]
            .into_iter()
            .find(|d| ok(d));
    }
    let mut starts = Vec::new();
    if let Ok(d) = std::env::current_dir() {
        starts.push(d);
    }
    if let Ok(e) = std::env::current_exe() {
        starts.push(e);
    }
    for s in starts {
        for a in s.ancestors() {
            let d = a.join("scripts");
            if ok(&d) {
                return Some(d);
            }
        }
    }
    None
}

impl Ctx {
    fn scripts(&self) -> Result<&Path> {
        self.scripts.as_deref().ok_or_else(|| {
            Error::Msg("scripts/map-region.py not found: run lehu-maps from a LEHU checkout, or give --scripts".into())
        })
    }

    fn venv(&self) -> PathBuf {
        self.cache.join("venv")
    }

    fn extract_dir(&self, area: &Area) -> PathBuf {
        self.cache
            .join("extracts")
            .join(geofabrik::folder(&area.id))
    }

    fn region_dir(&self, area: &Area) -> PathBuf {
        self.cache.join("regions").join(geofabrik::folder(&area.id))
    }

    /// A built region for (region, extract date, preset).
    pub fn build_dir(&self, area: &Area, ex: &Extract, preset: &str) -> PathBuf {
        let date = ex
            .date
            .clone()
            .unwrap_or_else(|| ex.md5.chars().take(8).collect());
        self.region_dir(area).join(format!("{date}-{preset}"))
    }

    /// The newest region built for `area`, any date, when there is one.
    pub fn newest_build(&self, area: &Area, preset: &str) -> Option<(PathBuf, Manifest)> {
        let mut found: Vec<PathBuf> = fs::read_dir(self.region_dir(area))
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().ends_with(&format!("-{preset}")))
            })
            .collect();
        found.sort();
        found.into_iter().rev().find_map(|p| {
            let m = Manifest::read(&p).ok()?;
            m.check_sizes(&p).ok()?;
            Some((p, m))
        })
    }

    /// The extracts already in the cache for `area`, newest first, with
    /// their md5s.
    pub fn cached_extracts(&self, area: &Area) -> Vec<Extract> {
        let mut v: Vec<Extract> = fs::read_dir(self.extract_dir(area))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let name = p.file_name()?.to_string_lossy().into_owned();
                if !name.ends_with(".osm.pbf") {
                    return None;
                }
                let md5 = fs::read_to_string(sidecar(&p)).ok()?;
                Some(Extract {
                    date: geofabrik::dated(&name),
                    md5: md5.trim().into(),
                    path: p,
                })
            })
            .collect();
        v.sort_by(|a, b| b.path.cmp(&a.path));
        v
    }

    /// The region's extract: Geofabrik's latest, downloaded unless the
    /// cache has the same file (by its md5). Offline, the newest cached.
    pub fn fetch_extract(&self, area: &Area, report: Report) -> Result<Extract> {
        report(Event::Stage(format!(
            "Downloading the OpenStreetMap extract of {}",
            area.name
        )));
        report(Event::Progress(-1.0, String::new()));
        let head = match self.http.head(&area.pbf) {
            Ok(h) => h,
            Err(e) => {
                if let Some(ex) = self.cached_extracts(area).into_iter().next() {
                    report(Event::Log(format!("{e}; using the extract from the cache")));
                    return Ok(ex);
                }
                return Err(e);
            }
        };
        let url = head.url;
        let file = url
            .rsplit('/')
            .next()
            .unwrap_or("extract.osm.pbf")
            .to_string();
        let file = if file.ends_with(".osm.pbf") {
            file
        } else {
            format!("{}.osm.pbf", geofabrik::folder(&area.id))
        };
        let md5 = self
            .http
            .text(&format!("{url}.md5"))
            .ok()
            .and_then(|t| http::parse_md5(&t))
            .or_else(|| {
                self.http
                    .text(&format!("{}.md5", area.pbf))
                    .ok()
                    .and_then(|t| http::parse_md5(&t))
            })
            .ok_or_else(|| {
                Error::Msg(format!("{url}.md5: Geofabrik gave no md5 for the extract"))
            })?;
        let dest = self.extract_dir(area).join(&file);
        let date = geofabrik::dated(&file);
        if dest.exists() && fs::read_to_string(sidecar(&dest)).is_ok_and(|s| s.trim() == md5) {
            report(Event::Log(format!("{file}: already downloaded")));
            return Ok(Extract {
                path: dest,
                date,
                md5,
            });
        }
        report(Event::Log(format!("Downloading {url}")));
        self.http
            .download(&url, &dest, Some(&md5), &self.cancel, &mut |done, total| {
                let (f, d) = match total {
                    Some(t) if t > 0 => (
                        done as f32 / t as f32,
                        format!("{} of {}", human_size(done), human_size(t)),
                    ),
                    _ => (-1.0, human_size(done)),
                };
                report(Event::Progress(f, d));
            })?;
        fs::write(sidecar(&dest), &md5).map_err(|e| io_err(dest.display(), e))?;
        // The older extracts of the region go.
        for old in self.cached_extracts(area) {
            if old.path != dest {
                let _ = fs::remove_file(sidecar(&old.path));
                let _ = fs::remove_file(&old.path);
            }
        }
        Ok(Extract {
            path: dest,
            date,
            md5,
        })
    }

    /// The venv's python, set up on first use.
    pub fn python(&self, report: Report) -> Result<PathBuf> {
        if let Some(py) = python::ready(&self.venv()) {
            return Ok(py);
        }
        report(Event::Stage(
            "Setting up Python for the map scripts (once)".into(),
        ));
        report(Event::Progress(-1.0, String::new()));
        python::ensure(&self.venv(), &self.cancel, &mut |l| {
            report(Event::Log(l.to_string()))
        })
    }

    fn region_cmd(
        &self,
        py: &Path,
        pbf: &Path,
        out: &Path,
        preset: &str,
    ) -> Result<std::process::Command> {
        let mut c = proc::command(py);
        c.arg(self.scripts()?.join("map-region.py"))
            .arg(pbf)
            .arg(out)
            .args(["--preset", preset, "--workers", &self.workers.to_string()]);
        Ok(c)
    }

    /// map-region.py's `--estimate`: bytes and seconds.
    pub fn estimate(&self, py: &Path, pbf: &Path, preset: &str) -> Result<(u64, u64)> {
        let tmp = self.cache.join("estimate");
        let mut c = self.region_cmd(py, pbf, &tmp, preset)?;
        c.arg("--estimate");
        let mut found = None;
        proc::run(c, &self.cancel, &mut |s, l| {
            if s == Stream::Out
                && let Line::Estimate(b, t) = progress::parse(l)
            {
                found = Some((b, t));
            }
            Ok(())
        })?;
        let _ = fs::remove_dir(&tmp);
        found.ok_or_else(|| Error::Msg("map-region.py --estimate printed no estimate".into()))
    }

    /// The region built, or found built in the cache.
    pub fn build_region(
        &self,
        py: &Path,
        area: &Area,
        ex: &Extract,
        preset: &str,
        report: Report,
    ) -> Result<(PathBuf, Manifest)> {
        let out = self.build_dir(area, ex, preset);
        if let Ok(m) = Manifest::read(&out)
            && m.check_sizes(&out).is_ok()
        {
            report(Event::Log(format!(
                "{}: already built ({})",
                area.name,
                out.display()
            )));
            return Ok((out, m));
        }
        let mut part = out.clone().into_os_string();
        part.push(".building");
        let part = PathBuf::from(part);
        let _ = fs::remove_dir_all(&part);
        let _ = fs::remove_dir_all(&out);
        if let Some(p) = part.parent() {
            fs::create_dir_all(p).map_err(|e| io_err(p.display(), e))?;
        }
        report(Event::Stage(format!("Building {}", area.name)));
        report(Event::Progress(0.0, String::new()));
        let mut c = self.region_cmd(py, &ex.path, &part, preset)?;
        c.arg("--progress");
        report(Event::Log(format!("{c:?}")));
        let mut stage = String::new();
        let mut done = false;
        let r = proc::run(c, &self.cancel, &mut |s, l| {
            if s == Stream::Err {
                report(Event::Log(l.to_string()));
                return Ok(());
            }
            match progress::parse(l) {
                Line::Stage(st) => {
                    report(Event::Stage(format!(
                        "{}: {}",
                        area.name,
                        progress::stage_text(&st)
                    )));
                    stage = st;
                }
                Line::Progress(d, t) => report(Event::Progress(
                    progress::overall(&stage, d, t),
                    if t > 1 {
                        format!("{d} of {t}")
                    } else {
                        String::new()
                    },
                )),
                Line::Done => done = true,
                Line::Error(m) => {
                    return Err(Error::Msg(format!("Building {} failed: {m}", area.name)));
                }
                Line::Estimate(..) => {}
                Line::Text(t) => report(Event::Log(t)),
            }
            Ok(())
        });
        let r = r.and_then(|()| {
            if !done {
                return Err(Error::Msg("map-region.py ended without @done".into()));
            }
            let m = Manifest::read(&part)?;
            m.check_sizes(&part)?;
            fs::rename(&part, &out).map_err(|e| io_err(out.display(), e))?;
            Ok(m)
        });
        match r {
            Ok(m) => {
                // Older builds of the region with this preset go.
                if let Ok(rd) = fs::read_dir(self.region_dir(area)) {
                    for e in rd.flatten() {
                        let p = e.path();
                        let n = e.file_name().to_string_lossy().into_owned();
                        if p != out && n.ends_with(&format!("-{preset}")) {
                            let _ = fs::remove_dir_all(p);
                        }
                    }
                }
                Ok((out, m))
            }
            Err(e) => {
                let _ = fs::remove_dir_all(&part);
                Err(e)
            }
        }
    }

    /// The voice's clips with their map.toml, made or found in the cache
    /// (by the voice and the phrases of voice-clips.py).
    pub fn build_voice(&self, py: &Path, v: &Voice, report: Report) -> Result<(PathBuf, Manifest)> {
        let script = self.scripts()?.join("voice-clips.py");
        let phrases = manifest::sha256_file(&script)?;
        let out = self
            .cache
            .join("voices")
            .join(format!("{}-{}", v.key, &phrases[..8]))
            .join(voice::LANG);
        if let Ok(m) = Manifest::read(&out)
            && m.check_sizes(&out).is_ok()
        {
            return Ok((out, m));
        }
        let model_dir = self.cache.join("piper").join(&v.key);
        for (path, size, md5) in [&v.onnx, &v.config] {
            let name = path.rsplit('/').next().unwrap_or(path);
            let dest = model_dir.join(name);
            if dest.exists() && fs::read_to_string(sidecar(&dest)).is_ok_and(|s| s.trim() == md5) {
                continue;
            }
            report(Event::Stage(format!("Downloading the voice {}", v.key)));
            self.http.download(
                &voice::url(path),
                &dest,
                Some(md5),
                &self.cancel,
                &mut |d, _| {
                    report(Event::Progress(
                        d as f32 / (*size).max(1) as f32,
                        format!("{} of {}", human_size(d), human_size(*size)),
                    ));
                },
            )?;
            fs::write(sidecar(&dest), md5).map_err(|e| io_err(dest.display(), e))?;
        }
        report(Event::Stage(format!(
            "Making the spoken prompts ({})",
            v.key
        )));
        report(Event::Progress(-1.0, String::new()));
        let _ = fs::remove_dir_all(&out);
        fs::create_dir_all(&out).map_err(|e| io_err(out.display(), e))?;
        let onnx = model_dir.join(v.onnx.0.rsplit('/').next().unwrap_or_default());
        let mut c = proc::command(py);
        c.arg(&script).arg(&onnx).arg(&out);
        let r = proc::run(c, &self.cancel, &mut |_, l| {
            report(Event::Log(l.to_string()));
            Ok(())
        })
        .and_then(|()| manifest::write_voice(&out, voice::LANG, &v.key));
        match r {
            Ok(m) => Ok((out, m)),
            Err(e) => {
                let _ = fs::remove_dir_all(&out);
                Err(e)
            }
        }
    }
}

/// `x.osm.pbf.md5`: the md5 a finished download was checked against.
fn sidecar(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".md5");
    PathBuf::from(s)
}

/// What "Write to card" does.
#[derive(Clone, Debug)]
pub struct Plan {
    pub area: Area,
    pub preset: String,
    pub voice: Option<Voice>,
    pub card: PathBuf,
}

/// Roughly what a voice set takes.
const VOICE_BYTES: u64 = 4_000_000;

/// Is there room on the card for `need` bytes (with a margin)?
pub fn room(card: &Path, need: u64) -> Result<()> {
    let Some((free, _)) = card::space(card) else {
        return Ok(());
    };
    let need = need + need / 20 + 16_000_000;
    if free < need {
        return Err(format!(
            "Not enough room on the card: this needs about {}, {} is free. Remove a region from the card first.",
            human_size(need),
            human_size(free)
        )
        .into());
    }
    Ok(())
}

/// The whole of "Write to card". What it wrote, for the window.
pub fn write_to_card(ctx: &Ctx, plan: &Plan, report: Report) -> Result<String> {
    let area = &plan.area;
    let folder = geofabrik::folder(&area.id);
    if !plan.card.is_dir() {
        return Err(format!(
            "{} is not there any more: is the card still in?",
            plan.card.display()
        )
        .into());
    }
    ctx.scripts()?;
    let ex = ctx.fetch_extract(area, report)?;
    ctx.cancel.check()?;
    let py = ctx.python(report)?;
    ctx.cancel.check()?;
    let voice_need = if plan.voice.is_some() { VOICE_BYTES } else { 0 };
    let out = ctx.build_dir(area, &ex, &plan.preset);
    if Manifest::read(&out).is_err() {
        report(Event::Stage("Estimating the size".into()));
        let (bytes, secs) = ctx.estimate(&py, &ex.path, &plan.preset)?;
        report(Event::Log(format!(
            "Estimate: {}, {}",
            human_size(bytes),
            crate::util::human_time(secs)
        )));
        room(&plan.card, bytes + voice_need)?;
    }
    let (built, m) = ctx.build_region(&py, area, &ex, &plan.preset, report)?;
    let voice = match &plan.voice {
        Some(v) => Some(ctx.build_voice(&py, v, report)?),
        None => None,
    };
    let mut wrote = Vec::new();
    if same_on_card(&plan.card, &folder, &m) {
        report(Event::Log(format!(
            "{} is on the card already, the same copy",
            area.name
        )));
    } else {
        room(&plan.card, m.size() + voice_need)?;
        copy(ctx, &built, &plan.card, &folder, &area.name, report)?;
        wrote.push(area.name.clone());
    }
    if let Some((dir, vm)) = voice {
        let rel = format!("{}/{}", card::VOICE, voice::LANG);
        if !same_on_card(&plan.card, &rel, &vm) {
            copy(ctx, &dir, &plan.card, &rel, "the voice", report)?;
            wrote.push("the voice".into());
        }
    }
    Ok(if wrote.is_empty() {
        format!("{} was on the card already. Nothing written.", area.name)
    } else {
        format!(
            "{} written and checked. Eject the card safely before you take it out.",
            wrote.join(" and ")
        )
    })
}

/// The card has this copy at `rel` already, whole (the same map.toml, and
/// every file at its size).
fn same_on_card(card: &Path, rel: &str, m: &Manifest) -> bool {
    card::resolve(card, rel)
        .is_some_and(|d| Manifest::read(&d).is_ok_and(|on| on == *m) && m.check_sizes(&d).is_ok())
}

fn copy(ctx: &Ctx, src: &Path, card: &Path, rel: &str, what: &str, report: Report) -> Result<()> {
    report(Event::Stage(format!("Writing {what} to the card")));
    let mut checking = false;
    install::install(src, card, rel, &ctx.cancel, &mut |s| match s {
        Step::Copy(d, t) => report(Event::Progress(
            d as f32 / t.max(1) as f32,
            format!("{} of {}", human_size(d), human_size(t)),
        )),
        Step::Verify(d, t) => {
            if !checking {
                checking = true;
                report(Event::Stage(format!("Checking {what} on the card")));
            }
            report(Event::Progress(
                d as f32 / t.max(1) as f32,
                format!("{} of {}", human_size(d), human_size(t)),
            ));
        }
    })?;
    report(Event::Log(format!(
        "{what}: written to {}",
        card.join(rel).display()
    )));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// A fake map-region.py and voice-clips.py, for the whole run without
    /// OpenStreetMap or Piper: the region is two small files and its
    /// map.toml, written the way the real one is.
    const FAKE_REGION: &str = r#"
import hashlib, os, sys, time
a = sys.argv[1:]
pbf, out = a[0], a[1]
if "--estimate" in a:
    print("@estimate 5000 3", flush=True); sys.exit(0)
if open(pbf, "rb").read() == b"broken":
    print("@stage graph", flush=True); print("@error the extract is broken", flush=True); sys.exit(1)
os.makedirs(out, exist_ok=True)
for st in ["graph", "tiles", "labels", "pack", "manifest"]:
    print("@stage " + st, flush=True)
    for i in range(3):
        print(f"@progress {i} 2", flush=True)
    print("working on " + st, file=sys.stderr, flush=True)
open(os.path.join(out, "roads.bin"), "wb").write(open(pbf, "rb").read())
open(os.path.join(out, "tiles.pmtiles"), "wb").write(b"\x07" * 4000)
lines = ['name = "Testland"', "data-version = 1", 'osm-date = "2026-11-10"', 'preset = "auto"', "[files]"]
for f in sorted(["roads.bin", "tiles.pmtiles"]):
    b = open(os.path.join(out, f), "rb").read()
    lines.append(f'"{f}" = {{ size = {len(b)}, sha256 = "{hashlib.sha256(b).hexdigest()}" }}')
open(os.path.join(out, "map.toml"), "w").write("\n".join(lines) + "\n")
print("@done", flush=True)
"#;

    fn fake_ctx(t: &TempDir) -> (Ctx, Area) {
        let scripts = t.path().join("scripts");
        fs::create_dir_all(&scripts).unwrap();
        fs::write(scripts.join("map-region.py"), FAKE_REGION).unwrap();
        let ctx = Ctx {
            cache: t.path().join("cache"),
            scripts: Some(scripts),
            http: Http::new(),
            cancel: Cancel::new(),
            workers: 1,
        };
        let area = Area {
            id: "testland".into(),
            parent: None,
            name: "Testland".into(),
            pbf: "http://127.0.0.1:9/testland-latest.osm.pbf".into(),
            path: String::new(),
            depth: 0,
            codes: vec![],
        };
        (ctx, area)
    }

    fn python3() -> Option<PathBuf> {
        python::find().ok()
    }

    #[test]
    fn builds_with_progress_and_caches() {
        let Some(py) = python3() else { return };
        let t = TempDir::new("jobs");
        let (ctx, area) = fake_ctx(&t);
        let pbf = t.path().join("testland-261110.osm.pbf");
        fs::write(&pbf, b"pbf bytes").unwrap();
        let ex = Extract {
            path: pbf,
            date: Some("2026-11-10".into()),
            md5: "x".into(),
        };
        assert_eq!(ctx.estimate(&py, &ex.path, "auto").unwrap(), (5000, 3));
        let mut events = Vec::new();
        let (dir, m) = ctx
            .build_region(&py, &area, &ex, "auto", &mut |e| events.push(e))
            .unwrap();
        assert!(dir.ends_with("regions/testland/2026-11-10-auto"));
        assert_eq!(m.name, "Testland");
        assert!(events.contains(&Event::Stage("Testland: Drawing the map tiles".into())));
        assert!(events.contains(&Event::Log("working on pack".into())));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Progress(f, _) if (*f - 1.0).abs() < 1e-6))
        );
        // Built once: the second time comes from the cache.
        let mut again = Vec::new();
        ctx.build_region(&py, &area, &ex, "auto", &mut |e| again.push(e))
            .unwrap();
        assert!(
            again
                .iter()
                .any(|e| matches!(e, Event::Log(l) if l.contains("already built")))
        );
        assert!(ctx.newest_build(&area, "auto").is_some());
        // And onto a card.
        let cardp = t.path().join("card");
        fs::create_dir_all(&cardp).unwrap();
        copy(&ctx, &dir, &cardp, "testland", "Testland", &mut |_| {}).unwrap();
        assert_eq!(card::contents(&cardp).regions[0].name, "Testland");
    }

    #[test]
    fn a_failed_build_says_why_and_leaves_nothing() {
        let Some(py) = python3() else { return };
        let t = TempDir::new("jobsfail");
        let (ctx, area) = fake_ctx(&t);
        let pbf = t.path().join("testland-261110.osm.pbf");
        fs::write(&pbf, b"broken").unwrap();
        let ex = Extract {
            path: pbf,
            date: None,
            md5: "abcdef0123".into(),
        };
        let e = ctx
            .build_region(&py, &area, &ex, "auto", &mut |_| {})
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "Building Testland failed: the extract is broken"
        );
        let dir = ctx.build_dir(&area, &ex, "auto");
        assert!(dir.ends_with("abcdef01-auto"));
        assert!(!dir.exists());
        assert_eq!(fs::read_dir(dir.parent().unwrap()).unwrap().count(), 0);
    }

    #[test]
    fn room_on_the_card() {
        let t = TempDir::new("room");
        room(t.path(), 1000).unwrap();
        let e = room(t.path(), u64::MAX / 4).unwrap_err();
        assert!(
            e.to_string().starts_with("Not enough room on the card"),
            "{e}"
        );
    }

    #[test]
    fn scripts_found() {
        let t = TempDir::new("scripts");
        assert_eq!(find_scripts(Some(t.path())), None);
        fs::create_dir_all(t.path().join("scripts")).unwrap();
        fs::write(t.path().join("scripts/map-region.py"), "").unwrap();
        assert_eq!(find_scripts(Some(t.path())), Some(t.path().join("scripts")));
        assert_eq!(
            find_scripts(Some(&t.path().join("scripts"))),
            Some(t.path().join("scripts"))
        );
    }
}
