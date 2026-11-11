// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! `--screenshot DIR`: the window in its states (nothing chosen, a region
//! chosen, building, done, an error) drawn by Slint's software renderer
//! offscreen into PNGs, from made-up data. Nothing is downloaded or built.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize, Rgb8Pixel};

use crate::MainWindow;
use crate::app::{self, App, Info};
use crate::card::{Card, Contents, Item};
use crate::geofabrik::Area;
use crate::voice;

struct Offscreen(Rc<MinimalSoftwareWindow>);

impl Platform for Offscreen {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

const W: u32 = 1040;
const H: u32 = 700;

fn area(id: &str, parent: Option<&str>, name: &str, path: &str, depth: usize) -> Area {
    Area {
        id: id.into(),
        parent: parent.map(String::from),
        name: name.into(),
        pbf: format!("https://download.geofabrik.de/{id}-latest.osm.pbf"),
        path: path.into(),
        depth,
        codes: vec![],
    }
}

fn sample() -> App {
    let mut areas = vec![
        area("africa", None, "Africa", "", 0),
        area("asia", None, "Asia", "", 0),
        area("europe", None, "Europe", "", 0),
    ];
    for c in [
        "Albania",
        "Andorra",
        "Austria",
        "Belgium",
        "Bosnia-Herzegovina",
        "Bulgaria",
        "Croatia",
        "Czech Republic",
        "Denmark",
        "Estonia",
        "Finland",
        "France",
        "Germany",
        "Greece",
        "Hungary",
        "Iceland",
        "Ireland and Northern Ireland",
        "Italy",
        "Latvia",
        "Lithuania",
        "Luxembourg",
        "Netherlands",
        "Poland",
    ] {
        areas.push(area(
            &c.to_lowercase().replace(' ', "-"),
            Some("europe"),
            c,
            "Europe",
            1,
        ));
    }
    areas.push(area("north-america", None, "North America", "", 0));
    let mut a = App {
        ctx: Some(crate::jobs::Ctx {
            cache: PathBuf::from("~/.cache/lehu-maps"),
            scripts: Some(PathBuf::from("~/lehu/scripts")),
            http: crate::http::Http::new(),
            cancel: crate::util::Cancel::new(),
            workers: 3,
        }),
        voices: voice::fallback(),
        voice: Some(0),
        ..App::default()
    };
    a.voices.push(voice::Voice {
        key: "en_US-lessac-medium".into(),
        title: "Lessac, American English, medium".into(),
        onnx: Default::default(),
        config: Default::default(),
    });
    a.set_areas(areas);
    a.log("Geofabrik's index: 554 regions");
    a
}

fn with_card(a: &mut App) {
    a.cards = vec![Card {
        root: "/media/me/LEHU-MAPS".into(),
        title: "LEHU-MAPS (/media/me/LEHU-MAPS)".into(),
        device: "/dev/sdb7".into(),
    }];
    a.card = Some(0);
    a.space = Some((26_400_000_000, 26_800_000_000));
    a.contents = Contents {
        regions: vec![Item {
            rel: "luxembourg".into(),
            name: "Luxembourg".into(),
            osm_date: "2026-11-08".into(),
            size: 34_215_652,
            problem: None,
        }],
        voices: vec![Item {
            rel: "voice/en".into(),
            name: "en".into(),
            osm_date: String::new(),
            size: 2_840_112,
            problem: None,
        }],
    };
}

fn select_hungary(a: &mut App) {
    a.selected = a.areas.iter().position(|x| x.id == "hungary");
    a.info.insert(
        "hungary".into(),
        Info {
            extract: "330 MB, OSM data of 2026-11-10".into(),
            size: "about 594 MB; building takes about 75 minutes".into(),
            need: Some(594_000_000),
        },
    );
}

fn render(window: &MinimalSoftwareWindow, path: &Path, (w, h): (u32, u32)) -> Result<(), String> {
    window.set_size(PhysicalSize::new(w, h));
    let mut buf = vec![Rgb8Pixel::default(); (w * h) as usize];
    // Twice: the list's rows are made in the first layout.
    for _ in 0..3 {
        slint::platform::update_timers_and_animations();
        window.request_redraw();
        window.draw_if_needed(|r| {
            r.render(&mut buf, w as usize);
        });
    }
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let bytes: Vec<u8> = buf.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
    enc.write_header()
        .and_then(|mut w| w.write_image_data(&bytes))
        .map_err(|e| format!("{}: {e}", path.display()))
}

pub fn run(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Offscreen(window.clone())))
        .map_err(|e| e.to_string())?;
    let ui = MainWindow::new().map_err(|e| e.to_string())?;
    ui.set_version(concat!("v", env!("CARGO_PKG_VERSION")).into());
    window.set_size(PhysicalSize::new(W, H));
    ui.show().map_err(|e| e.to_string())?;

    let shot_at =
        |name: &str, a: &mut App, log_open: bool, size: (u32, u32)| -> Result<(), String> {
            ui.set_search_text(a.query.clone().into());
            app::apply(&ui, a);
            ui.set_log_open(log_open);
            ui.invoke_scroll_to_top();
            let path = dir.join(format!("{name}.png"));
            render(&window, &path, size)?;
            println!("{}", path.display());
            Ok(())
        };
    let shot = |name: &str, a: &mut App, log_open: bool| shot_at(name, a, log_open, (W, H));

    // The smallest window, the log open: the right side scrolls.
    let mut a = sample();
    with_card(&mut a);
    select_hungary(&mut a);
    a.busy = true;
    a.stage = "Writing Hungary to the card".into();
    a.progress = 0.61;
    a.detail = "362 MB of 594 MB".into();
    shot_at("7-small-window", &mut a, true, (900, 600))?;

    let mut a = sample();
    shot("1-empty", &mut a, false)?;

    let mut a = sample();
    with_card(&mut a);
    select_hungary(&mut a);
    shot("2-region-selected", &mut a, false)?;

    let mut a = sample();
    with_card(&mut a);
    a.filter("hun");
    select_hungary(&mut a);
    a.busy = true;
    a.stage = "Hungary: Drawing the map tiles".into();
    a.progress = 0.15 + 0.55 * 0.42;
    a.detail = "41800 of 99409".into();
    for l in [
        "== Downloading the OpenStreetMap extract of Hungary",
        "Downloading https://download.geofabrik.de/europe/hungary-261110.osm.pbf",
        "== Estimating the size",
        "Estimate: 594 MB, about 75 minutes",
        "== Building Hungary",
        "== Hungary: Building the road network",
        "+ python scripts/map-graph.py hungary-261110.osm.pbf ...",
        "roads.bin: 1201330 junctions, 1452807 roads, 480 speed cameras; 68.6 MB",
        "== Hungary: Drawing the map tiles",
    ] {
        a.log(l);
    }
    shot("3-building", &mut a, true)?;

    let mut a = sample();
    with_card(&mut a);
    select_hungary(&mut a);
    a.contents.regions.insert(
        0,
        Item {
            rel: "hungary".into(),
            name: "Hungary".into(),
            osm_date: "2026-11-10".into(),
            size: 594_212_331,
            problem: None,
        },
    );
    a.space = Some((25_800_000_000, 26_800_000_000));
    a.status =
        "Hungary and the voice written and checked. Eject the card safely before you take it out."
            .into();
    a.status_kind = 1;
    shot("4-done", &mut a, false)?;

    let mut a = sample();
    with_card(&mut a);
    select_hungary(&mut a);
    a.status = "Building Hungary failed: map-graph.py failed (exit 1)".into();
    a.status_kind = 2;
    a.contents.regions[0].problem = Some("damaged: roads.bin is 5242880 bytes, not 7043480".into());
    shot("5-error", &mut a, true)?;

    let mut a = sample();
    a.selected = a.areas.iter().position(|x| x.id == "luxembourg");
    a.python_problem = Some("No Python 3 found: the map scripts need it; install python3 and python3-venv with your package manager".into());
    shot("6-no-python-no-card", &mut a, false)?;

    Ok(())
}
