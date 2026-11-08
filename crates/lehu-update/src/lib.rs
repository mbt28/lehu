//! Settings > Update: LEHU OS updates, installed by the image's
//! `/usr/sbin/lehu-os-update` (LEHU OS only; elsewhere the screens say it is
//! not available).
//!
//! The board has no internet of its own, so the phone carries the update.
//! It joins the head unit's access point and opens the page LEHU serves
//! there; the page fetches the update server's manifest and the update over
//! the phone's own network and uploads the file. The phone is only a
//! courier: the page is plain http (no `crypto.subtle`), so the head unit
//! checks the size and SHA-256 the manifest gave while it receives, and
//! `lehu-os-update check` verifies the update's signature before anything
//! is installed.
//!
//! [`Updater`] is what the program holds. The installer's commands run on a
//! worker thread (`check` takes seconds, `apply` minutes); the page is
//! served by one thread, started when the user opens "Get from phone" and
//! stopped when an install starts, ten minutes after the Update pages were
//! left, or with LEHU. Both change one [`View`] behind a lock and call the
//! program's wake; the loop copies the view when its generation moved.

mod parse;
mod server;
mod store;

pub use parse::{Checked, Mode, Progress, Status, failure, fields, sentence};
pub use server::page;
pub use store::{MAX_SIZE, valid_name};

use std::collections::VecDeque;
use std::fs;
use std::io::Read;
use std::net::{Ipv4Addr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

/// The image's installer.
pub const INSTALLER: &str = "/usr/sbin/lehu-os-update";
/// Where `apply` says how far it is.
pub const PROGRESS: &str = "/run/lehu-os-update/progress";
/// Where an update is received: on the SD card (the image makes it 1770
/// root:lehu). Never /tmp or /var/tmp: they are RAM.
pub const CACHE: &str = "/var/cache/lehu-update";
/// The page stops this long after the Update pages were left.
pub const IDLE_STOP: Duration = Duration::from_secs(600);
/// The installer restarts the head unit within this after it ended; if not,
/// the screens say so and the phones may come back.
pub const RESTART_WAIT: Duration = Duration::from_secs(120);

/// Where everything is; the program's is [`Config::system`], tests point
/// it at a folder and a stand-in installer.
#[derive(Clone, Debug)]
pub struct Config {
    pub installer: PathBuf,
    pub progress: PathBuf,
    pub cache: PathBuf,
    /// The outcome last shown after a restart (`time=`, `commit=`).
    pub shown: PathBuf,
    /// `check` and `apply` through `sudo -n` (LEHU is not root).
    pub sudo: bool,
    /// What the page fetches; the updates' files are relative to it.
    pub manifest_url: String,
    pub port: u16,
    /// The installer's unit runs (an `apply` LEHU did not see start).
    pub unit_running: fn() -> bool,
    pub idle_stop: Duration,
    /// How long the restart after a good `apply` is waited for.
    pub restart_wait: Duration,
    /// A connection that sends nothing this long is dropped.
    pub read_timeout: Duration,
}

impl Config {
    /// The image's paths; the last outcome shown is kept in LEHU's state
    /// directory.
    pub fn system(state_dir: &Path, manifest_url: &str, port: u16) -> Self {
        Self {
            installer: INSTALLER.into(),
            progress: PROGRESS.into(),
            cache: CACHE.into(),
            shown: state_dir.join("update-shown"),
            sudo: !is_root(),
            manifest_url: manifest_url.to_string(),
            port,
            unit_running,
            idle_stop: IDLE_STOP,
            restart_wait: RESTART_WAIT,
            read_timeout: Duration::from_secs(30),
        }
    }
}

fn is_root() -> bool {
    // SAFETY: geteuid takes nothing and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// `systemctl is-active lehu-os-update`: an apply runs in a unit of that
/// name. No systemctl: none runs.
fn unit_running() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", "lehu-os-update"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The installer is there: LEHU OS.
pub fn available(cfg: &Config) -> bool {
    cfg.installer.is_file()
}

/// Where the update is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stage {
    /// Nothing received, or a part of it (`received` of `size`) waiting for
    /// the phone to send the rest.
    #[default]
    Idle,
    Receiving,
    /// Received whole; `check` runs.
    Checking,
    /// Checked: `version`, `mode`, `size_kb` say what it is.
    Ready,
    /// The transfer, the check or the install failed: `message` says why.
    Failed,
    /// `apply` runs: `percent`, `message`.
    Installing,
    /// The head unit restarts by itself (into the update boot, or the new
    /// version).
    Restarting,
}

impl Stage {
    pub fn word(self) -> &'static str {
        match self {
            Stage::Idle => "idle",
            Stage::Receiving => "receiving",
            Stage::Checking => "checking",
            Stage::Ready => "ready",
            Stage::Failed => "failed",
            Stage::Installing => "installing",
            Stage::Restarting => "restarting",
        }
    }
}

/// How the last update went, found at start and shown once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It runs now: its version.
    Updated(String),
    /// Why it did not go in.
    Failed(String),
    /// Cut off (power lost): it can be installed again.
    Interrupted,
}

/// Everything the screens show of the update, and the page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    /// The installer is there.
    pub available: bool,
    /// The commit running and its version, from `status`.
    pub booted: String,
    pub booted_version: String,
    pub stage: Stage,
    /// The update's file, while one is received or held.
    pub file: String,
    pub received: u64,
    pub size: u64,
    /// The manifest's line about it, as the page passed it on.
    pub notes: String,
    /// From `check`.
    pub version: String,
    pub mode: Mode,
    pub size_kb: u64,
    /// While installing.
    pub percent: u32,
    /// Why it failed, or the install's step.
    pub message: String,
    /// The page is served, at `url`; or why not.
    pub serving: bool,
    pub url: String,
    pub serve_error: String,
    pub outcome: Option<Outcome>,
}

enum Job {
    /// `status`; at start also the outcome, the clean up and a leftover.
    Status {
        at_start: bool,
    },
    /// `check` the upload; `quiet`: a leftover found at start, nothing to
    /// say if it fails.
    Check {
        tar: PathBuf,
        quiet: bool,
    },
    Apply {
        tar: PathBuf,
    },
}

/// What the threads share.
pub(crate) struct Shared {
    pub(crate) cfg: Config,
    inner: Mutex<Inner>,
    work: Condvar,
    generation: AtomicU32,
    notify: Box<dyn Fn() + Send + Sync>,
}

pub(crate) struct Inner {
    pub(crate) view: View,
    jobs: VecDeque<Job>,
    /// The Update pages were left then; `None` while they show.
    pub(crate) left_at: Option<Instant>,
    /// The page's server: its stop switch, and the connection it serves
    /// (cut when it stops).
    pub(crate) server_stop: Option<Arc<AtomicBool>>,
    pub(crate) conn: Option<TcpStream>,
    /// The outcome shown: written to `cfg.shown` once seen.
    shown_pending: Option<(String, String)>,
    /// The last wake for a transfer's progress.
    pub(crate) noted_at: Option<Instant>,
}

/// A transfer's progress wakes the loop at most this often.
const NOTE_EVERY: Duration = Duration::from_millis(250);

impl Shared {
    pub(crate) fn new(cfg: Config, notify: Box<dyn Fn() + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                view: View {
                    available: available(&cfg),
                    ..View::default()
                },
                jobs: VecDeque::new(),
                left_at: Some(Instant::now()),
                server_stop: None,
                conn: None,
                shown_pending: None,
                noted_at: None,
            }),
            cfg,
            work: Condvar::new(),
            generation: AtomicU32::new(1),
            notify,
        })
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Change the view and tell the loop.
    pub(crate) fn change<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        let out = {
            let mut inner = self.lock();
            let out = f(&mut inner);
            inner.noted_at = Some(Instant::now());
            out
        };
        self.generation.fetch_add(1, Ordering::Release);
        (self.notify)();
        out
    }

    /// A transfer's progress: the view changes every time, the loop hears
    /// of it a few times a second.
    pub(crate) fn progress(&self, f: impl FnOnce(&mut View)) {
        let tell = {
            let mut inner = self.lock();
            f(&mut inner.view);
            let due = inner.noted_at.is_none_or(|t| t.elapsed() >= NOTE_EVERY);
            if due {
                inner.noted_at = Some(Instant::now());
            }
            due
        };
        if tell {
            self.generation.fetch_add(1, Ordering::Release);
            (self.notify)();
        }
    }

    pub(crate) fn push(&self, inner: &mut Inner, job: Job) {
        inner.jobs.push_back(job);
        self.work.notify_one();
    }

    /// The received update, whole: checked next.
    pub(crate) fn received(&self, inner: &mut Inner, file: &str) {
        inner.view.stage = Stage::Checking;
        inner.view.file = file.to_string();
        inner.view.message.clear();
        let tar = self.cfg.cache.join(file);
        self.push(inner, Job::Check { tar, quiet: false });
    }
}

/// The update as the program holds it.
pub struct Updater {
    shared: Arc<Shared>,
}

impl Updater {
    /// Ready for the screens; on LEHU OS, `status` is read at once on the
    /// worker, and what it says about the last update follows. `notify`
    /// wakes the loop.
    pub fn new(cfg: Config, notify: impl Fn() + Send + Sync + 'static) -> Self {
        let shared = Shared::new(cfg, Box::new(notify));
        if shared.lock().view.available {
            let worker = Arc::clone(&shared);
            let spawned = std::thread::Builder::new()
                .name("lehu-update".into())
                .stack_size(128 * 1024)
                .spawn(move || work(&worker));
            match spawned {
                Ok(_) => {
                    let mut inner = shared.lock();
                    shared.push(&mut inner, Job::Status { at_start: true });
                }
                Err(e) => {
                    log::warn!("update: no worker thread ({e})");
                    shared.lock().view.available = false;
                }
            }
        }
        Self { shared }
    }

    /// Moves with every change of the view.
    pub fn generation(&self) -> u32 {
        self.shared.generation.load(Ordering::Acquire)
    }

    /// A copy of the view.
    pub fn view(&self) -> View {
        self.shared.lock().view.clone()
    }

    /// "Get from phone" opened: the page served on the access point's
    /// address. Said in the view if it cannot be.
    pub fn serve(&self, ip: Option<Ipv4Addr>) {
        let shared = &self.shared;
        {
            let inner = shared.lock();
            if !inner.view.available || inner.server_stop.is_some() {
                return;
            }
        }
        let Some(ip) = ip else {
            shared.change(|inner| {
                inner.view.serving = false;
                inner.view.serve_error = "The access point is not running".into();
            });
            return;
        };
        match server::start(Arc::clone(shared), ip) {
            Ok(url) => {
                log::info!("update: the page is at {url}");
                shared.change(|inner| {
                    inner.view.serving = true;
                    inner.view.url = url;
                    inner.view.serve_error.clear();
                });
            }
            Err(e) => {
                log::warn!("update: cannot serve the page on {ip}: {e}");
                shared.change(|inner| {
                    inner.view.serving = false;
                    inner.view.serve_error = "The page cannot be served".into();
                });
            }
        }
    }

    /// The Update pages show (or were left): ten minutes after they were
    /// left, with nothing in flight, the page stops.
    pub fn screens(&self, visible: bool) {
        let mut inner = self.shared.lock();
        if visible {
            inner.left_at = None;
        } else if inner.left_at.is_none() {
            inner.left_at = Some(Instant::now());
        }
    }

    /// Install the checked update: the page stops, `apply` runs. False
    /// when there is nothing checked to install.
    pub fn install(&self) -> bool {
        let shared = &self.shared;
        shared.change(|inner| {
            if inner.view.stage != Stage::Ready || inner.view.file.is_empty() {
                return false;
            }
            server::stop(inner);
            inner.view.stage = Stage::Installing;
            inner.view.percent = 0;
            inner.view.message = "Starting".into();
            let tar = shared.cfg.cache.join(&inner.view.file);
            log::info!("update: installing {}", tar.display());
            shared.push(inner, Job::Apply { tar });
            true
        })
    }

    /// The outcome was shown: not again after the next start.
    pub fn outcome_seen(&self) {
        let pending = self.shared.change(|inner| {
            inner.view.outcome = None;
            inner.shown_pending.take()
        });
        if let Some((time, commit)) = pending
            && let Err(e) = write_shown(&self.shared.cfg.shown, &time, &commit)
        {
            log::warn!("update: {}: {e}", self.shared.cfg.shown.display());
        }
    }

    /// An install runs or the head unit is about to restart: the phone's
    /// sessions stay off.
    pub fn busy(&self) -> bool {
        matches!(
            self.shared.lock().view.stage,
            Stage::Installing | Stage::Restarting
        )
    }
}

impl Drop for Updater {
    fn drop(&mut self) {
        server::stop(&mut self.shared.lock());
    }
}

/// The worker: one job at a time, for as long as the program runs.
fn work(shared: &Shared) {
    loop {
        let job = {
            let mut inner = shared.lock();
            loop {
                if let Some(job) = inner.jobs.pop_front() {
                    break job;
                }
                inner = shared.work.wait(inner).unwrap_or_else(|e| e.into_inner());
            }
        };
        match job {
            Job::Status { at_start } => run_status(shared, at_start),
            Job::Check { tar, quiet } => run_check(shared, &tar, quiet),
            Job::Apply { tar } => run_apply(shared, &tar),
        }
    }
}

/// The installer with `args`; `check` and `apply` need root.
fn installer(cfg: &Config, args: &[&std::ffi::OsStr], root: bool) -> Command {
    let mut c = if root && cfg.sudo {
        let mut c = Command::new("sudo");
        c.arg("-n").arg(&cfg.installer);
        c
    } else {
        Command::new(&cfg.installer)
    };
    c.args(args).stdin(Stdio::null());
    c
}

fn run_status(shared: &Shared, at_start: bool) {
    let cfg = &shared.cfg;
    let status = match installer(cfg, &["status".as_ref()], false).output() {
        Ok(out) => Status::parse(&String::from_utf8_lossy(&out.stdout)),
        Err(e) => {
            log::warn!("update: {} status: {e}", cfg.installer.display());
            return;
        }
    };
    log::info!(
        "update: running {} ({}), last update: {}",
        status.booted_version,
        short(&status.booted),
        if status.state.is_empty() {
            "none"
        } else {
            &status.state
        }
    );
    let outcome = if at_start {
        let shown = read_shown(&cfg.shown);
        outcome_of(&status, &shown, cfg.unit_running)
    } else {
        None
    };
    let cleaned = matches!(outcome, Some(Outcome::Updated(_)));
    if cleaned {
        // Installed and running: what was received is not needed now.
        store::remove_others(&cfg.cache, &[]);
    }
    let leftover = (at_start && !cleaned)
        .then(|| store::leftover(&cfg.cache))
        .flatten();
    shared.change(|inner| {
        inner.view.booted = status.booted.clone();
        inner.view.booted_version = status.booted_version.clone();
        if let Some(o) = outcome {
            log::info!("update: the last update: {o:?}");
            inner.view.outcome = Some(o);
            inner.shown_pending = Some((status.time.clone(), status.commit.clone()));
        }
        // An update received earlier and not installed is checked again,
        // so it can be installed now.
        if let Some(tar) = leftover {
            log::info!("update: {} was received before", tar.display());
            if let Some(name) = tar.file_name().and_then(|n| n.to_str()) {
                inner.view.file = name.to_string();
            }
            inner.view.stage = Stage::Checking;
            shared.push(inner, Job::Check { tar, quiet: true });
        }
    });
}

/// What to say about the last update, if it is news: an outcome newer
/// than the one last shown (`time=` and `commit=`).
fn outcome_of(s: &Status, shown: &(String, String), unit: fn() -> bool) -> Option<Outcome> {
    if s.state.is_empty() || (s.time == shown.0 && s.commit == shown.1) {
        return None;
    }
    match s.state.as_str() {
        "installed" if !s.commit.is_empty() && s.booted == s.commit => {
            Some(Outcome::Updated(s.version.clone()))
        }
        "installed" => Some(Outcome::Failed(format!(
            "Version {} is installed, but the version before started",
            s.version
        ))),
        "failed" if s.message.is_empty() => Some(Outcome::Failed("No reason given".into())),
        "failed" => Some(Outcome::Failed(sentence(&s.message))),
        "installing" | "checking" if !unit() => Some(Outcome::Interrupted),
        _ => None,
    }
}

fn read_shown(path: &Path) -> (String, String) {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut shown = (String::new(), String::new());
    for (name, value) in fields(&text) {
        match name {
            "time" => shown.0 = value.to_string(),
            "commit" => shown.1 = value.to_string(),
            _ => {}
        }
    }
    shown
}

fn write_shown(path: &Path, time: &str, commit: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, format!("time={time}\ncommit={commit}\n"))?;
    fs::rename(&tmp, path)
}

fn run_check(shared: &Shared, tar: &Path, quiet: bool) {
    log::info!("update: checking {}", tar.display());
    let out = installer(&shared.cfg, &["check".as_ref(), tar.as_os_str()], true).output();
    let (stdout, stderr, code) = match &out {
        Ok(o) => (
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
            o.status.code(),
        ),
        Err(e) => (String::new(), e.to_string(), None),
    };
    let checked = out
        .as_ref()
        .is_ok_and(|o| o.status.success())
        .then(|| Checked::parse(&stdout))
        .flatten();
    shared.change(|inner| {
        let v = &mut inner.view;
        match checked {
            Some(c) => {
                log::info!(
                    "update: {} is version {}, {} KB, {}",
                    tar.display(),
                    c.version,
                    c.size_kb,
                    c.mode.word()
                );
                v.stage = Stage::Ready;
                v.version = c.version;
                v.mode = c.mode;
                v.size_kb = c.size_kb;
                v.message.clear();
            }
            None => {
                let why = failure(&stdout, &stderr, code);
                log::warn!("update: check {}: {why}", tar.display());
                if quiet {
                    v.stage = Stage::Idle;
                    v.file.clear();
                } else {
                    v.stage = Stage::Failed;
                    v.message = why;
                }
            }
        }
    });
}

/// A child's output, read on a thread of its own so a full pipe never
/// stops it; at most `CAP` bytes are kept.
fn collect(pipe: Option<impl Read + Send + 'static>) -> Option<std::thread::JoinHandle<String>> {
    const CAP: usize = 64 * 1024;
    let mut pipe = pipe?;
    std::thread::Builder::new()
        .name("lehu-update-out".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            let mut kept = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let room = CAP.saturating_sub(kept.len()).min(n);
                kept.extend_from_slice(&buf[..room]);
            }
            String::from_utf8_lossy(&kept).into_owned()
        })
        .ok()
}

/// The progress file is read this often while `apply` runs.
const PROGRESS_EVERY: Duration = Duration::from_millis(500);

fn run_apply(shared: &Shared, tar: &Path) {
    let cfg = &shared.cfg;
    let started = SystemTime::now() - Duration::from_secs(1);
    let mut cmd = installer(cfg, &["apply".as_ref(), tar.as_os_str()], true);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("update: apply: {e}");
            shared.change(|inner| {
                inner.view.stage = Stage::Failed;
                inner.view.message = sentence(&e.to_string());
            });
            return;
        }
    };
    let out = collect(child.stdout.take());
    let err = collect(child.stderr.take());
    let mut seen = Progress::default();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(e) => {
                log::warn!("update: apply: {e}");
                break None;
            }
        }
        // Only the file this apply wrote: one from an earlier run is stale.
        let fresh = fs::metadata(&cfg.progress)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t >= started);
        if fresh && let Ok(text) = fs::read_to_string(&cfg.progress) {
            let p = Progress::parse(&text);
            if p != seen {
                seen = p.clone();
                shared.change(|inner| {
                    let v = &mut inner.view;
                    v.percent = p.percent;
                    if !p.message.is_empty() {
                        v.message = p.message;
                    }
                    if p.phase == "update-boot" {
                        v.stage = Stage::Restarting;
                    }
                });
            }
        }
        std::thread::sleep(PROGRESS_EVERY);
    };
    let stdout = out.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = err.and_then(|h| h.join().ok()).unwrap_or_default();
    let ok = status.is_some_and(|s| s.success());
    log::info!(
        "update: apply ended ({}): {}",
        status.map_or("?".to_string(), |s| s.to_string()),
        stdout.trim().replace('\n', "; ")
    );
    shared.change(|inner| {
        let v = &mut inner.view;
        if ok {
            // The installer restarts the head unit by itself.
            v.stage = Stage::Restarting;
            v.percent = 100;
            if v.message.is_empty() || v.mode == Mode::UpdateBoot {
                v.message = match v.mode {
                    Mode::InPlace => "Installed. Restarting".into(),
                    Mode::UpdateBoot => "Restarting to install the update".into(),
                };
            }
        } else {
            v.stage = Stage::Failed;
            v.message = failure(&stdout, &stderr, status.and_then(|s| s.code()));
            // The version running, as it is now.
            shared.push(inner, Job::Status { at_start: false });
        }
    });
    if ok {
        // Nothing else is queued while it restarts: the worker can wait.
        std::thread::sleep(cfg.restart_wait);
        log::warn!("update: the head unit did not restart");
        shared.change(|inner| {
            let v = &mut inner.view;
            v.stage = Stage::Failed;
            v.message = "The head unit did not restart. \
                         The update takes effect at the next start"
                .into();
        });
    }
}

fn short(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

#[cfg(test)]
mod tests;
