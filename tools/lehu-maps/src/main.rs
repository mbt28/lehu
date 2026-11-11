// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! lehu-maps: puts LEHU's map regions and voice on the head unit's SD card
//! from a PC. It downloads an OpenStreetMap extract from Geofabrik, builds
//! the region with the repository's scripts/map-region.py in a Python
//! environment of its own, makes the spoken prompts with Piper, and writes
//! both to the card's LEHU-MAPS partition, checked. See README.md.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod card;
mod geofabrik;
mod http;
mod install;
mod jobs;
mod manifest;
mod proc;
mod progress;
mod python;
mod shots;
#[cfg(test)]
mod testutil;
mod util;
mod voice;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::ComponentHandle;

slint::include_modules!();

const HELP: &str = "\
lehu-maps: maps and the voice for LEHU's SD card

    lehu-maps [--scripts DIR] [--workers N] [--card DIR]
    lehu-maps --write REGION --card DIR [--voice KEY|none] [--scripts DIR] [--workers N]
    lehu-maps --screenshot DIR

  --scripts DIR     the folder with map-region.py (or a LEHU checkout);
                    found by itself when run from inside a checkout
  --workers N       processes drawing tiles (default: the cores less one)
  --card DIR        a folder to write to, instead of a LEHU-MAPS drive
  --write REGION    without the window: write Geofabrik's REGION (its id,
                    e.g. luxembourg) to --card as Write to card does, with
                    the voice --voice (en_GB-cori-high unless named)
  --screenshot DIR  draw the window's states into DIR as PNGs, and exit

The cache (extracts, the Python environment, built regions) is in
LEHU_MAPS_CACHE, else the platform's cache folder.";

#[derive(Default)]
struct Args {
    scripts: Option<PathBuf>,
    workers: Option<usize>,
    card: Option<PathBuf>,
    screenshot: Option<PathBuf>,
    write: Option<String>,
    voice: Option<String>,
}

fn args() -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = std::env::args_os().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.to_string_lossy().as_ref() {
            "--scripts" => a.scripts = Some(value("--scripts")?.into()),
            "--card" => a.card = Some(value("--card")?.into()),
            "--screenshot" => a.screenshot = Some(value("--screenshot")?.into()),
            "--write" => a.write = Some(value("--write")?.to_string_lossy().into_owned()),
            "--voice" => a.voice = Some(value("--voice")?.to_string_lossy().into_owned()),
            "--workers" => {
                let v = value("--workers")?;
                a.workers = Some(
                    v.to_string_lossy()
                        .parse()
                        .ok()
                        .filter(|&n: &usize| n > 0)
                        .ok_or("--workers needs a number")?,
                );
            }
            "-h" | "--help" => return Err(HELP.into()),
            other => return Err(format!("{other}: not an option\n\n{HELP}")),
        }
    }
    Ok(a)
}

fn main() {
    let a = match args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(if e == HELP { 0 } else { 2 });
        }
    };
    if let Some(dir) = &a.screenshot {
        if let Err(e) = shots::run(dir) {
            eprintln!("lehu-maps: {e}");
            std::process::exit(1);
        }
        return;
    }
    if a.write.is_some() {
        match headless(&a) {
            Ok(msg) => println!("{msg}"),
            Err(e) => {
                eprintln!("lehu-maps: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if let Err(e) = run(a) {
        eprintln!("lehu-maps: {e}");
        std::process::exit(1);
    }
}

fn ctx(a: &Args) -> jobs::Ctx {
    let workers = a.workers.unwrap_or_else(|| {
        std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1).max(1))
    });
    jobs::Ctx {
        cache: util::cache_dir(),
        scripts: jobs::find_scripts(a.scripts.as_deref()),
        http: http::Http::new(),
        cancel: util::Cancel::new(),
        workers,
    }
}

/// `--write`: what Write to card does, its stages and progress on stderr.
fn headless(a: &Args) -> Result<String, String> {
    let id = a.write.as_deref().unwrap_or_default();
    let card = a.card.clone().ok_or("--write needs --card DIR")?;
    let ctx = ctx(a);
    let index = ctx
        .http
        .text(geofabrik::INDEX_URL)
        .map_err(|e| e.to_string())?;
    let area = geofabrik::parse(&index)?
        .into_iter()
        .find(|x| x.id == id)
        .ok_or_else(|| format!("{id}: not in Geofabrik's index"))?;
    let voice = match a.voice.as_deref() {
        Some("none") => None,
        want => {
            let key = want.unwrap_or(voice::DEFAULT);
            let list = ctx
                .http
                .text(voice::VOICES_URL)
                .ok()
                .and_then(|t| voice::parse(&t).ok())
                .unwrap_or_else(voice::fallback);
            Some(
                list.into_iter()
                    .find(|v| v.key == key)
                    .ok_or_else(|| format!("{key}: no such English voice"))?,
            )
        }
    };
    let plan = jobs::Plan {
        preset: geofabrik::preset(&area.id).into(),
        voice,
        card,
        area,
    };
    let mut last = -1i32;
    jobs::write_to_card(&ctx, &plan, &mut |e| match e {
        jobs::Event::Stage(s) => {
            last = -1;
            eprintln!("== {s}");
        }
        jobs::Event::Log(l) => eprintln!("   {l}"),
        jobs::Event::Progress(f, d) => {
            let pct = if f < 0.0 { -1 } else { (f * 10.0) as i32 * 10 };
            if pct != last && pct >= 0 {
                eprintln!("   {pct}% {d}");
            }
            last = pct;
        }
    })
    .map_err(|e| e.to_string())
}

fn run(a: Args) -> Result<(), slint::PlatformError> {
    let ctx = ctx(&a);
    let mut state = app::App {
        voices: voice::fallback(),
        voice: Some(0),
        ..app::App::default()
    };
    state.log(format!("Cache: {}", ctx.cache.display()));
    match &ctx.scripts {
        Some(s) => state.log(format!("Build scripts: {}", s.display())),
        None => state.log("scripts/map-region.py not found: give --scripts DIR"),
    }
    if let Some(c) = &a.card {
        let card = card::folder(c);
        state.folder = Some(card.clone());
        state.folder_text = c.display().to_string();
        state.cards.push(card);
        state.card = Some(0);
    }
    state.ctx = Some(ctx);
    let shared: app::Shared = Arc::new(Mutex::new(state));

    let ui = MainWindow::new()?;
    ui.set_version(concat!("v", env!("CARGO_PKG_VERSION")).into());
    if let Some(c) = &a.card {
        ui.set_folder_path(c.display().to_string().into());
    }
    app::connect(&ui, &shared);
    app::apply(&ui, &mut app::lock(&shared));
    app::load_index(ui.as_weak(), shared.clone());
    app::load_voices(ui.as_weak(), shared.clone());
    app::check_python(ui.as_weak(), shared.clone());
    app::refresh_cards(ui.as_weak(), shared.clone());

    // A card put in or taken out shows within a few seconds.
    let timer = slint::Timer::default();
    let (w, s) = (ui.as_weak(), shared.clone());
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(3),
        move || {
            if !app::lock(&s).busy {
                app::refresh_cards(w.clone(), s.clone());
            }
        },
    );
    ui.run()
}
