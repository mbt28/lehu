// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The window's state and what its buttons do. The state lives behind a
//! mutex; the workers change it from their threads through
//! `slint::invoke_from_event_loop` ([`post`]), and [`apply`] puts it into
//! the window.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use crate::card::{self, Card, Contents};
use crate::geofabrik::{self, Area};
use crate::jobs::{self, Ctx, Event, Plan};
use crate::python;
use crate::util::{Cancel, Error, human_size, human_time};
use crate::voice::{self, Voice};
use crate::{AreaRow, CardItem, MainWindow, VoiceRow};

/// What the window knows of a region before it is built.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub extract: String,
    pub size: String,
    /// The built size, or the estimate, for the room on the card.
    pub need: Option<u64>,
}

#[derive(Default)]
pub struct App {
    pub ctx: Option<Ctx>,
    pub areas: Vec<Area>,
    /// The rows shown (indices into `areas`), for the search.
    pub shown: Vec<usize>,
    pub query: String,
    pub areas_dirty: bool,
    pub selected: Option<usize>,
    pub info: HashMap<String, Info>,
    pub index_status: String,
    pub index_failed: bool,

    pub voices: Vec<Voice>,
    /// The voices ticked, by key, in the order they were.
    pub chosen: Vec<String>,

    pub cards: Vec<Card>,
    pub card: Option<usize>,
    /// The folder chosen instead of a drive.
    pub folder: Option<Card>,
    pub folder_text: String,
    pub contents: Contents,
    pub space: Option<(u64, u64)>,

    pub python_problem: Option<String>,

    pub busy: bool,
    pub cancel: Option<Cancel>,
    pub stage: String,
    pub progress: f32,
    pub detail: String,
    pub status: String,
    pub status_kind: i32,
    pub log: VecDeque<String>,
}

pub type Shared = Arc<Mutex<App>>;

const LOG_LINES: usize = 400;

pub fn lock(s: &Shared) -> MutexGuard<'_, App> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

impl App {
    pub fn log(&mut self, line: impl Into<String>) {
        self.log.push_back(line.into());
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }

    pub fn area(&self) -> Option<&Area> {
        self.selected.and_then(|i| self.areas.get(i))
    }

    /// The voices ticked, in the list's order.
    pub fn chosen_voices(&self) -> Vec<Voice> {
        self.voices
            .iter()
            .filter(|v| self.chosen.contains(&v.key))
            .cloned()
            .collect()
    }

    /// Tick or untick the voice at `i` of the list.
    pub fn toggle_voice(&mut self, i: usize) {
        let Some(key) = self.voices.get(i).map(|v| v.key.clone()) else {
            return;
        };
        if let Some(at) = self.chosen.iter().position(|k| *k == key) {
            self.chosen.remove(at);
        } else {
            self.chosen.push(key);
        }
    }

    pub fn set_areas(&mut self, areas: Vec<Area>) {
        let id = self.area().map(|a| a.id.clone());
        self.areas = areas;
        self.selected = id.and_then(|id| self.areas.iter().position(|a| a.id == id));
        self.filter(&self.query.clone());
    }

    pub fn filter(&mut self, q: &str) {
        self.query = q.to_string();
        self.shown = geofabrik::filter(&self.areas, q);
        self.areas_dirty = true;
    }

    /// The card chosen: a drive, else the folder.
    pub fn target(&self) -> Option<&Card> {
        self.card.and_then(|i| self.cards.get(i))
    }

    pub fn set_cards(&mut self, found: Vec<Card>) {
        let keep = self.target().map(|c| c.root.clone());
        self.cards = found;
        if let Some(f) = &self.folder
            && !self.cards.iter().any(|c| c.root == f.root)
        {
            self.cards.push(f.clone());
        }
        self.card = keep
            .and_then(|r| self.cards.iter().position(|c| c.root == r))
            .or(if self.cards.is_empty() { None } else { Some(0) });
    }

    /// Why Write cannot be pressed, when it cannot.
    pub fn write_reason(&self) -> Option<String> {
        if self.busy {
            return Some("Working…".into());
        }
        if self.ctx.as_ref().is_none_or(|c| c.scripts.is_none()) {
            return Some(
                "scripts/map-region.py not found: start LEHU Maps from a LEHU checkout, or with --scripts DIR"
                    .into(),
            );
        }
        if let Some(p) = &self.python_problem {
            return Some(p.clone());
        }
        if self.area().is_none() && self.chosen.is_empty() {
            return Some("Choose a region on the left, or a voice.".into());
        }
        let Some(card) = self.target() else {
            return Some(format!(
                "Put the card in: no {} drive found. Or type the path of a folder.",
                card::LABEL
            ));
        };
        if let (Some(area), Some((free, _))) = (self.area(), self.space)
            && let Some(need) = self.info.get(&area.id).and_then(|i| i.need)
        {
            let on_card = self
                .contents
                .regions
                .iter()
                .find(|r| r.rel == geofabrik::folder(&area.id))
                .map_or(0, |r| r.size);
            if need > free + on_card {
                return Some(format!(
                    "Not enough room on {}: {} needs about {}, {} is free.",
                    card.title,
                    area.name,
                    human_size(need),
                    human_size(free)
                ));
            }
        }
        None
    }
}

/// "No voice: no spoken prompts", "Cori, British English, high", "2 voices:
/// Cori, Anna".
pub fn voices_chosen(v: &[Voice]) -> String {
    let first = |v: &Voice| v.title.split(',').next().unwrap_or_default().to_string();
    match v {
        [] => "No voice: no spoken prompts.".into(),
        [one] => one.title.clone(),
        many => format!(
            "{} voices: {}",
            many.len(),
            many.iter().map(first).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn preset_text(p: &str) -> String {
    match p {
        "luxembourg" => "luxembourg: zoom 8–14, and 15–16 over Luxembourg City".into(),
        "hungary" => {
            "hungary: zoom 8–14, and 15–16 over Budapest and the towns of 50,000 or more".into()
        }
        _ => "auto: zoom 8–14, and 15–16 over the towns of 50,000 or more".into(),
    }
}

/// The state into the window.
pub fn apply(ui: &MainWindow, app: &mut App) {
    if std::mem::take(&mut app.areas_dirty) {
        let rows: Vec<AreaRow> = app
            .shown
            .iter()
            .map(|&i| {
                let a = &app.areas[i];
                AreaRow {
                    name: a.name.clone().into(),
                    path: if app.query.trim().is_empty() {
                        SharedString::new()
                    } else {
                        a.path.clone().into()
                    },
                    depth: a.depth as i32,
                }
            })
            .collect();
        ui.set_areas(ModelRc::new(VecModel::from(rows)));
    }
    let row = app
        .selected
        .and_then(|s| app.shown.iter().position(|&i| i == s))
        .map_or(-1, |r| r as i32);
    ui.set_selected_row(row);
    ui.set_index_status(app.index_status.clone().into());
    ui.set_index_failed(app.index_failed);

    match app.area() {
        Some(a) => {
            let info = app.info.get(&a.id).cloned().unwrap_or_default();
            ui.set_sel_name(a.name.clone().into());
            ui.set_sel_path(
                if a.path.is_empty() {
                    format!("Geofabrik: {}", a.id)
                } else {
                    format!("{} · Geofabrik: {}", a.path, a.id)
                }
                .into(),
            );
            ui.set_sel_extract(if info.extract.is_empty() {
                "…".into()
            } else {
                info.extract.into()
            });
            ui.set_sel_size(if info.size.is_empty() {
                "…".into()
            } else {
                info.size.into()
            });
            ui.set_sel_preset(preset_text(geofabrik::preset(&a.id)).into());
        }
        None => {
            ui.set_sel_name(SharedString::new());
        }
    }

    let voices: Vec<VoiceRow> = app
        .voices
        .iter()
        .map(|v| VoiceRow {
            title: v.title.clone().into(),
            checked: app.chosen.contains(&v.key),
        })
        .collect();
    if !same(&ui.get_voices(), &voices) {
        ui.set_voices(ModelRc::new(VecModel::from(voices)));
    }
    ui.set_voices_chosen(voices_chosen(&app.chosen_voices()).into());

    let cards: Vec<SharedString> = app.cards.iter().map(|c| c.title.clone().into()).collect();
    if !same(&ui.get_cards(), &cards) {
        ui.set_cards(ModelRc::new(VecModel::from(cards)));
    }
    ui.set_card_index(app.card.map_or(-1, |c| c as i32));
    ui.set_card_space(match (app.target(), app.space) {
        (None, _) => format!(
            "No {} drive found. Put the head unit's card in the PC's reader; the drive appears in a few seconds.",
            card::LABEL
        ),
        (Some(_), Some((free, total))) => format!("{} free of {}", human_size(free), human_size(total)),
        (Some(_), None) => String::new(),
    }
    .into());
    let mut items: Vec<CardItem> = Vec::new();
    for r in &app.contents.regions {
        items.push(CardItem {
            rel: r.rel.clone().into(),
            title: r.name.clone().into(),
            detail: r
                .problem
                .clone()
                .unwrap_or_else(|| {
                    let date = if r.osm_date.is_empty() {
                        String::new()
                    } else {
                        format!("OSM {} · ", r.osm_date)
                    };
                    format!("{date}{} · /{}", human_size(r.size), r.rel)
                })
                .into(),
            problem: r.problem.is_some(),
        });
    }
    for v in &app.contents.voices {
        items.push(CardItem {
            rel: v.rel.clone().into(),
            title: format!("Voice {}", v.name).into(),
            detail: v
                .problem
                .clone()
                .unwrap_or_else(|| format!("{} · /{}", human_size(v.size), v.rel))
                .into(),
            problem: v.problem.is_some(),
        });
    }
    ui.set_card_note(
        if app.target().is_some() && items.is_empty() {
            "Nothing on the card yet."
        } else {
            ""
        }
        .into(),
    );
    if !same(&ui.get_card_items(), &items) {
        ui.set_card_items(ModelRc::new(VecModel::from(items)));
    }

    let reason = app.write_reason();
    ui.set_can_write(reason.is_none());
    ui.set_write_reason(reason.unwrap_or_default().into());
    ui.set_busy(app.busy);
    ui.set_stage(app.stage.clone().into());
    ui.set_progress(app.progress);
    ui.set_detail(app.detail.clone().into());
    ui.set_status(app.status.clone().into());
    ui.set_status_kind(app.status_kind);
    let log: Vec<&str> = app.log.iter().map(String::as_str).collect();
    let log = log.join("\n");
    if ui.get_log_text() != log.as_str() {
        ui.set_log_text(log.into());
    }
}

/// A model the same as `rows`: left as it is, so that an open list stays
/// open.
fn same<T: Clone + PartialEq + 'static>(m: &ModelRc<T>, rows: &[T]) -> bool {
    use slint::Model;
    m.row_count() == rows.len() && m.iter().zip(rows).all(|(a, b)| a == *b)
}

/// Change the state from any thread, then show it.
pub fn post(weak: &Weak<MainWindow>, shared: &Shared, f: impl FnOnce(&mut App) + Send + 'static) {
    let shared = shared.clone();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        let mut app = lock(&shared);
        f(&mut app);
        apply(&ui, &mut app);
    });
}

/// A worker's events, sent on to the window at most every 100 ms (and at
/// once for a new stage).
struct Reporter {
    weak: Weak<MainWindow>,
    shared: Shared,
    lines: Vec<String>,
    stage: Option<String>,
    progress: Option<(f32, String)>,
    sent: Instant,
}

impl Reporter {
    fn new(weak: Weak<MainWindow>, shared: Shared) -> Self {
        Self {
            weak,
            shared,
            lines: Vec::new(),
            stage: None,
            progress: None,
            sent: Instant::now(),
        }
    }

    fn event(&mut self, e: Event) {
        let urgent = matches!(e, Event::Stage(_));
        match e {
            Event::Stage(s) => {
                self.lines.push(format!("== {s}"));
                self.stage = Some(s);
                self.progress = Some((-1.0, String::new()));
            }
            Event::Progress(f, d) => self.progress = Some((f, d)),
            Event::Log(l) => self.lines.push(l),
        }
        if urgent || self.sent.elapsed() >= Duration::from_millis(100) {
            self.flush();
        }
    }

    fn flush(&mut self) {
        self.sent = Instant::now();
        let lines = std::mem::take(&mut self.lines);
        let stage = self.stage.take();
        let progress = self.progress.take();
        if lines.is_empty() && stage.is_none() && progress.is_none() {
            return;
        }
        post(&self.weak, &self.shared, move |app| {
            for l in lines {
                app.log(l);
            }
            if let Some(s) = stage {
                app.stage = s;
            }
            if let Some((f, d)) = progress {
                app.progress = f;
                app.detail = d;
            }
        });
    }
}

/// Geofabrik's index: from the cache when it is under a day old, else
/// fetched (and the cached one used when that fails).
pub fn load_index(weak: Weak<MainWindow>, shared: Shared) {
    post(&weak, &shared, |app| {
        app.index_status = "Loading Geofabrik's list of regions…".into();
        app.index_failed = false;
    });
    let (http, cache) = {
        let app = lock(&shared);
        let ctx = app.ctx.as_ref().expect("ctx");
        (ctx.http.clone(), ctx.cache.clone())
    };
    std::thread::spawn(move || {
        let path = cache.join("geofabrik-index.json");
        let fresh = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .is_ok_and(|t| {
                SystemTime::now().duration_since(t).unwrap_or_default()
                    < Duration::from_secs(86_400)
            });
        let cached = || std::fs::read_to_string(&path).ok();
        let text = if fresh {
            cached().ok_or_else(|| "the cached index went".to_string())
        } else {
            match http.text(geofabrik::INDEX_URL) {
                Ok(t) => {
                    let _ = std::fs::create_dir_all(&cache);
                    let _ = std::fs::write(&path, &t);
                    Ok(t)
                }
                Err(e) => cached().ok_or_else(|| e.to_string()),
            }
        };
        let parsed = text.and_then(|t| geofabrik::parse(&t));
        post(&weak, &shared, move |app| match parsed {
            Ok(areas) => {
                app.log(format!("Geofabrik's index: {} regions", areas.len()));
                app.index_status = String::new();
                app.set_areas(areas);
            }
            Err(e) => {
                app.log(e.clone());
                app.index_status = format!("Could not load the list of regions: {e}");
                app.index_failed = true;
            }
        });
    });
}

/// Piper's voices (the English ones); the default alone when the list
/// cannot be had.
pub fn load_voices(weak: Weak<MainWindow>, shared: Shared) {
    let (http, cache) = {
        let app = lock(&shared);
        let ctx = app.ctx.as_ref().expect("ctx");
        (ctx.http.clone(), ctx.cache.clone())
    };
    std::thread::spawn(move || {
        let path = cache.join("piper-voices.json");
        let text = http
            .text(voice::VOICES_URL)
            .inspect(|t| {
                let _ = std::fs::create_dir_all(&cache);
                let _ = std::fs::write(&path, t);
            })
            .ok()
            .or_else(|| std::fs::read_to_string(&path).ok());
        let voices = text
            .and_then(|t| voice::parse(&t).ok())
            .filter(|v| !v.is_empty());
        post(&weak, &shared, move |app| {
            match voices {
                Some(v) => app.voices = v,
                None => app
                    .log("Piper's voice list could not be had: only the default voice is offered"),
            }
            // The voices ticked stay ticked, when the list has them.
            let keys: Vec<String> = app.voices.iter().map(|v| v.key.clone()).collect();
            app.chosen.retain(|k| keys.contains(k));
        });
    });
}

pub fn check_python(weak: Weak<MainWindow>, shared: Shared) {
    std::thread::spawn(move || {
        let venv_ok = lock(&shared)
            .ctx
            .as_ref()
            .is_some_and(|c| python::ready(&c.cache.join("venv")).is_some());
        let problem = if venv_ok {
            None
        } else {
            python::find().err().map(|e| e.to_string())
        };
        post(&weak, &shared, move |app| app.python_problem = problem);
    });
}

/// The drives, and what is on the chosen one.
pub fn refresh_cards(weak: Weak<MainWindow>, shared: Shared) {
    std::thread::spawn(move || {
        let found = card::detect();
        let (root, changed) = {
            let mut app = lock(&shared);
            let before = app.cards.clone();
            app.set_cards(found);
            (app.target().map(|c| c.root.clone()), before != app.cards)
        };
        let contents = root.as_ref().map(|r| card::contents(r)).unwrap_or_default();
        let space = root.as_ref().and_then(|r| card::space(r));
        post(&weak, &shared, move |app| {
            if changed {
                app.log(match app.cards.len() {
                    0 => format!("No {} drive", card::LABEL),
                    _ => format!(
                        "Cards: {}",
                        app.cards
                            .iter()
                            .map(|c| c.title.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            }
            if app.target().map(|c| &c.root) == root.as_ref() {
                app.contents = contents;
                app.space = space;
            }
        });
    });
}

/// What the window can tell of a region before writing: the extract's
/// date and size, and the built size or an estimate.
pub fn load_info(weak: Weak<MainWindow>, shared: Shared, area: Area) {
    let Some(ctx) = lock(&shared).ctx.clone() else {
        return;
    };
    std::thread::spawn(move || {
        let preset = geofabrik::preset(&area.id);
        let mut info = Info::default();
        match ctx.http.head(&area.pbf) {
            Ok(h) => {
                let date = geofabrik::dated(&h.url)
                    .map(|d| format!(", OSM data of {d}"))
                    .unwrap_or_default();
                info.extract = match h.size {
                    Some(s) => format!("{}{date}", human_size(s)),
                    None => format!("size unknown{date}"),
                };
            }
            Err(e) => info.extract = format!("could not ask Geofabrik: {e}"),
        }
        if let Some((_, m)) = ctx.newest_build(&area, preset) {
            info.need = Some(m.size());
            info.size = format!(
                "{} (built from OSM data of {}; in the cache, so writing is quick)",
                human_size(m.size()),
                m.osm_date
            );
        } else if let (Some(py), Some(ex)) = (
            python::ready(&ctx.cache.join("venv")),
            ctx.cached_extracts(&area).into_iter().next(),
        ) {
            match ctx.estimate(&py, &ex.path, preset) {
                Ok((b, s)) => {
                    info.need = Some(b);
                    info.size =
                        format!("about {}; building takes {}", human_size(b), human_time(s));
                }
                Err(e) => info.size = format!("no estimate: {e}"),
            }
        } else {
            info.size = "known once the extract is downloaded; checked against the card's room before building".into();
        }
        post(&weak, &shared, move |app| {
            app.info.insert(area.id.clone(), info);
        });
    });
}

/// "Write to card".
pub fn write(weak: Weak<MainWindow>, shared: Shared) {
    let (ctx, plan) = {
        let mut app = lock(&shared);
        if app.write_reason().is_some() {
            return;
        }
        let (area, Some(card), Some(ctx)) =
            (app.area().cloned(), app.target().cloned(), app.ctx.clone())
        else {
            return;
        };
        let cancel = Cancel::new();
        let mut ctx = ctx;
        ctx.cancel = cancel.clone();
        app.cancel = Some(cancel);
        app.busy = true;
        app.status.clear();
        app.status_kind = 0;
        app.stage = "Starting".into();
        app.progress = -1.0;
        app.detail.clear();
        let plan = Plan {
            preset: area
                .as_ref()
                .map_or("auto", |a| geofabrik::preset(&a.id))
                .into(),
            voices: app.chosen_voices(),
            card: card.root.clone(),
            area,
        };
        let what = plan.area.as_ref().map_or_else(
            || voices_chosen(&plan.voices),
            |a| format!("{} and {}", a.name, voices_chosen(&plan.voices)),
        );
        app.log(format!("Writing {what} to {}", card.title));
        (ctx, plan)
    };
    post(&weak, &shared, |_| {});
    std::thread::spawn(move || {
        let mut rep = Reporter::new(weak.clone(), shared.clone());
        let r = jobs::write_to_card(&ctx, &plan, &mut |e| rep.event(e));
        rep.flush();
        let id = plan.area.as_ref().map(|a| a.id.clone());
        post(&weak, &shared, move |app| {
            app.busy = false;
            app.cancel = None;
            match r {
                Ok(msg) => {
                    app.log(msg.clone());
                    app.status = msg;
                    app.status_kind = 1;
                }
                Err(Error::Cancelled) => {
                    app.log("Cancelled");
                    app.status = "Cancelled. Nothing on the card was changed; a download goes on from where it stopped next time.".into();
                    app.status_kind = 3;
                }
                Err(Error::Msg(m)) => {
                    app.log(format!("Error: {m}"));
                    app.status = m;
                    app.status_kind = 2;
                }
            }
            if let Some(id) = &id {
                app.info.remove(id);
            }
        });
        refresh_cards(weak.clone(), shared.clone());
        let area = plan
            .area
            .as_ref()
            .and_then(|p| lock(&shared).areas.iter().find(|a| a.id == p.id).cloned());
        if let Some(a) = area {
            load_info(weak, shared, a);
        }
    });
}

/// Wire the window's callbacks.
pub fn connect(ui: &MainWindow, shared: &Shared) {
    let (w, s) = (ui.as_weak(), shared.clone());
    ui.on_search(move |q| post(&w, &s, move |app| app.filter(&q)));
    let (w, s) = (ui.as_weak(), shared.clone());
    ui.on_select(move |row| {
        let area = {
            let mut app = lock(&s);
            let Some(&i) = usize::try_from(row).ok().and_then(|r| app.shown.get(r)) else {
                return;
            };
            app.selected = Some(i);
            app.status.clear();
            let a = app.areas[i].clone();
            (!app.info.contains_key(&a.id)).then_some(a)
        };
        post(&w, &s, |_| {});
        if let Some(a) = area {
            load_info(w.clone(), s.clone(), a);
        }
    });
    let (w, s) = (ui.as_weak(), shared.clone());
    ui.on_retry_index(move || load_index(w.clone(), s.clone()));
    let (w, s) = (ui.as_weak(), shared.clone());
    ui.on_voice_toggled(move |i| {
        post(&w, &s, move |app| {
            if let Ok(i) = usize::try_from(i) {
                app.toggle_voice(i);
            }
        });
    });
    let (w, s) = (ui.as_weak(), shared.clone());
    ui.on_clear_region(move || {
        post(&w, &s, |app| {
            app.selected = None;
            app.areas_dirty = true;
            app.status.clear();
        });
    });
    let (w, s) = (ui.as_weak(), shared.clone());
    ui.on_card_changed(move |i| {
        {
            let mut app = lock(&s);
            app.card = usize::try_from(i).ok().filter(|&i| i < app.cards.len());
            app.contents = Contents::default();
            app.space = None;
        }
        refresh_cards(w.clone(), s.clone());
    });
    let (w, s) = (ui.as_weak(), shared.clone());
    ui.on_refresh_cards(move || refresh_cards(w.clone(), s.clone()));
    let (w, s) = (ui.as_weak(), shared.clone());
    ui.on_use_folder(move |path| {
        let p = PathBuf::from(path.trim());
        {
            let mut app = lock(&s);
            app.folder_text = path.to_string();
            if p.is_dir() {
                let c = card::folder(&p);
                app.folder = Some(c.clone());
                let mut cards = app.cards.clone();
                cards.retain(|x| x.root != c.root);
                cards.push(c);
                app.cards = cards;
                app.card = Some(app.cards.len() - 1);
                app.status.clear();
            } else {
                app.status = format!("{} is not a folder.", p.display());
                app.status_kind = 2;
            }
        }
        refresh_cards(w.clone(), s.clone());
    });
    let (w, s) = (ui.as_weak(), shared.clone());
    ui.on_remove_item(move |rel| {
        let root = lock(&s).target().map(|c| c.root.clone());
        if let Some(root) = root {
            let r = card::remove(&root, &rel);
            post(&w, &s, move |app| match r {
                Ok(()) => {
                    app.log(format!("Removed /{rel} from the card"));
                    app.status = format!("Removed /{rel} from the card.");
                    app.status_kind = 0;
                }
                Err(e) => {
                    app.status = format!("Could not remove /{rel}: {e}");
                    app.status_kind = 2;
                }
            });
            refresh_cards(w.clone(), s.clone());
        }
    });
    let (w, s) = (ui.as_weak(), shared.clone());
    ui.on_write(move || write(w.clone(), s.clone()));
    let s = shared.clone();
    ui.on_cancel(move || {
        if let Some(c) = &lock(&s).cancel {
            c.cancel();
        }
    });
}
