// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The Python the build scripts run in: a venv of the tool's own in the
//! cache, made from the system's Python 3 on first use, with the packages
//! the scripts import.

use std::path::{Path, PathBuf};

use crate::proc::{self, Stream};
use crate::util::{Cancel, Error, Result, io_err};

/// What scripts/map-region.py, map-tiles.py, map-graph.py and
/// voice-clips.py import.
pub const PACKAGES: &[&str] = &["osmium", "pillow", "numpy", "piper-tts"];

/// The oldest Python the scripts run on (tomllib).
const LEAST: (u32, u32) = (3, 11);

/// The marker a finished install leaves, with what it installed.
const READY: &str = "lehu-maps-ready";

/// `Python 3.13.5` → (3, 13).
pub fn parse_version(text: &str) -> Option<(u32, u32)> {
    let v = text.trim().strip_prefix("Python ")?;
    let mut it = v.split('.');
    Some((it.next()?.parse().ok()?, it.next()?.trim().parse().ok()?))
}

fn version_of(python: &Path) -> Option<(u32, u32)> {
    let out = proc::command(python).arg("--version").output().ok()?;
    let text = if out.stdout.is_empty() {
        out.stderr
    } else {
        out.stdout
    };
    parse_version(&String::from_utf8_lossy(&text))
}

/// The system's Python 3: `LEHU_MAPS_PYTHON`, else python3 (and on
/// Windows python, then the py launcher's), the first that runs and is
/// new enough. An error that says what to install, when there is none.
pub fn find() -> Result<PathBuf> {
    let mut tried: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("LEHU_MAPS_PYTHON") {
        tried.push(p.into());
    }
    if cfg!(windows) {
        tried.extend(["python", "python3", "py"].map(PathBuf::from));
    } else {
        tried.extend(["python3", "python"].map(PathBuf::from));
    }
    let mut old = None;
    for p in tried {
        match version_of(&p) {
            Some(v) if v >= LEAST => return Ok(p),
            Some(v) => old = Some(v),
            None => {}
        }
    }
    let hint = if cfg!(windows) {
        "install it from python.org (tick \"Add python.exe to PATH\")"
    } else if cfg!(target_os = "macos") {
        "install it from python.org or with Homebrew (brew install python)"
    } else {
        "install python3 and python3-venv with your package manager"
    };
    Err(match old {
        Some((a, b)) => format!(
            "Python {a}.{b} is too old: the map scripts need Python {}.{} or later; {hint}",
            LEAST.0, LEAST.1
        ),
        None => format!("No Python 3 found: the map scripts need it; {hint}"),
    }
    .into())
}

/// The venv's own python.
pub fn venv_python(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

/// The venv when it is ready, without making it.
pub fn ready(venv: &Path) -> Option<PathBuf> {
    let py = venv_python(venv);
    let marker = std::fs::read_to_string(venv.join(READY)).ok()?;
    (py.exists() && marker.trim() == PACKAGES.join(" ")).then_some(py)
}

/// The venv's python, the venv made and the packages installed first when
/// it is not ready. `log` has pip's lines.
pub fn ensure(venv: &Path, cancel: &Cancel, log: &mut dyn FnMut(&str)) -> Result<PathBuf> {
    if let Some(py) = ready(venv) {
        return Ok(py);
    }
    let system = find()?;
    let py = venv_python(venv);
    if !py.exists() {
        log(&format!(
            "Making a Python environment in {}",
            venv.display()
        ));
        let mut c = proc::command(&system);
        c.args(["-m", "venv"]).arg(venv);
        proc::run(c, cancel, &mut |_, l| {
            log(l);
            Ok(())
        })
        .map_err(|e| match e {
            Error::Cancelled => e,
            Error::Msg(m) => Error::Msg(format!(
                "Could not make a Python environment ({m}). On Debian or Ubuntu: \
                 sudo apt install python3-venv"
            )),
        })?;
    }
    log(&format!(
        "Installing {} (once; a few minutes)",
        PACKAGES.join(", ")
    ));
    let mut c = proc::command(&py);
    c.args([
        "-m",
        "pip",
        "install",
        "--disable-pip-version-check",
        "--progress-bar",
        "off",
    ]);
    c.args(PACKAGES);
    proc::run(c, cancel, &mut |s, l| {
        if s == Stream::Out || !l.trim().is_empty() {
            log(l);
        }
        Ok(())
    })
    .map_err(|e| match e {
        Error::Cancelled => e,
        Error::Msg(m) => Error::Msg(format!("Installing the Python packages failed: {m}")),
    })?;
    std::fs::write(venv.join(READY), PACKAGES.join(" ")).map_err(|e| io_err(venv.display(), e))?;
    Ok(py)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("Python 3.13.5\n"), Some((3, 13)));
        assert_eq!(parse_version("Python 3.9"), Some((3, 9)));
        assert_eq!(parse_version("bash: python: not found"), None);
        assert!((3, 9) < LEAST && (3, 13) >= LEAST);
    }

    #[test]
    fn venv_paths() {
        let p = venv_python(Path::new("v"));
        assert!(p.ends_with(if cfg!(windows) {
            "python.exe"
        } else {
            "python"
        }));
        assert_eq!(ready(Path::new("/nonexistent/venv")), None);
    }
}
