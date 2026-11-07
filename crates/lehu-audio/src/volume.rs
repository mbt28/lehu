//! The output volume: 0-100 % plus mute, kept across restarts in the user
//! settings. Applied through the sound card's own playback control when
//! it has one (ALSA's simple mixer, so any Linux card: "Master", "PCM",
//! "Headphone"...), mapped through the control's dB scale so that equal
//! steps sound about equal; otherwise, or when configured so, as a
//! software gain the mixer applies.
//!
//! Callable from any thread: keys arrive on the input thread, the screens
//! on the loop's, the mixer asks for the gain every period.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use lehu_core::clock;
use lehu_core::settings::Audio;

use crate::alsa::{Elem, SimpleMixer};
use crate::fixed::{self, DB_GAIN_MUTE};

/// How "auto" picks the control that sets the volume. Inputs mixed into
/// the output ("Mic", "Line In"...) have playback volumes too, so nothing
/// outside these lists is guessed: a card with none of them gets the
/// software gain.
/// 1. A control in front of every output.
const SHARED: [&str; 2] = ["Master", "PCM"];
/// 2. A card's only output amplifier (the F1C codec's "Headphone"). With
///    several, Speaker and Headphone say, none of them is the volume.
const OUTPUTS: [&str; 7] = [
    "Headphone",
    "HP",
    "Line Out",
    "Lineout",
    "OUT",
    "Speaker",
    "Earpiece",
];
/// 3. A digital volume feeding every output.
const DIGITAL: [&str; 3] = ["Digital", "DAC", "Playback"];

/// ALSA PCM aliases whose first positional argument is the card
/// (alsa-lib's conf/pcm/*.conf).
const BY_CARD: [&str; 19] = [
    "hw",
    "plughw",
    "sysdefault",
    "default",
    "front",
    "rear",
    "center_lfe",
    "side",
    "surround21",
    "surround40",
    "surround41",
    "surround50",
    "surround51",
    "surround71",
    "iec958",
    "spdif",
    "hdmi",
    "dmix",
    "dsnoop",
];

/// A changed level is saved once it has been left alone this long, so
/// holding a volume key does not rewrite the settings file at every step.
const SAVE_AFTER_MS: u64 = 2000;

/// The control device of the card an ALSA PCM name plays on: "hw:0,0",
/// "plughw:1", "front:CARD=Device,DEV=0", "plug:hw:1" -> "hw:<card>";
/// "default" when the name does not say (a plugin such as "pulse" has its
/// volume on the default control device).
pub fn mixer_device_for(pcm: &str) -> String {
    let pcm: String = pcm.chars().filter(|&c| c != '"').collect(); // plug:"hw:1,0"
    let Some((kind, args)) = pcm.split_once(':') else {
        return "default".to_string();
    };
    if let Some(named) = args.find("CARD=") {
        let rest = &args[named + 5..];
        let card = rest.split(',').next().unwrap_or_default();
        return if card.is_empty() {
            "default".to_string()
        } else {
            format!("hw:CARD={card}")
        };
    }
    if args.contains(':') {
        return mixer_device_for(args); // a plugin over another PCM
    }
    if !BY_CARD.contains(&kind) {
        return "default".to_string();
    }
    let card = args.split(',').next().unwrap_or_default();
    if card.is_empty() {
        "default".to_string()
    } else {
        format!("hw:{card}")
    }
}

/// `level` 1..=100 onto the control, through its dB scale when it has one.
/// 0 is the bottom of its raw range.
fn set_level(e: &Elem<'_>, level: u32) -> bool {
    if level == 0 {
        return e.volume_range().is_some_and(|(min, _)| e.set_volume(min));
    }
    match e.db_range() {
        Some((min, max)) if min < max => e.set_db(fixed::db_of_level(level, min, max)),
        _ => e
            .volume_range()
            .is_some_and(|(min, max)| e.set_volume(fixed::raw_of_level(level, min, max))),
    }
}

/// The control's current setting as a level, the inverse of `set_level`;
/// None when it cannot be read.
fn get_level(e: &Elem<'_>) -> Option<u32> {
    match e.db_range() {
        Some((min, max)) if min < max => Some(fixed::level_of_db(e.db()?, min, max)),
        _ => {
            let (min, max) = e.volume_range()?;
            if min >= max {
                return None;
            }
            Some(fixed::level_of_raw(e.volume()?, min, max))
        }
    }
}

/// The control "auto" settles on, by name, or None (see the lists above).
fn pick_control(m: &mut SimpleMixer) -> Option<String> {
    for name in SHARED {
        if let Some(e) = m.find(name) {
            return Some(e.name());
        }
    }
    let mut only = None;
    let mut outputs = 0;
    for name in OUTPUTS {
        if let Some(e) = m.find(name) {
            only = Some(e.name());
            outputs += 1;
        }
    }
    if outputs == 1 {
        return only;
    }
    for name in DIGITAL {
        if let Some(e) = m.find(name) {
            return Some(e.name());
        }
    }
    None
}

/// "-62.5" for -6250.
fn db_text(centi: i64) -> String {
    let sign = if centi < 0 { "-" } else { "" };
    let a = centi.abs();
    format!("{sign}{}.{}", a / 100, (a % 100) / 10)
}

struct Inner {
    /// The ALSA control device.
    device: String,
    /// The volume-control setting.
    wanted: String,
    /// The chosen control's name (empty: software).
    control: String,
    mixer: Option<SimpleMixer>,
    has_switch: bool,
    /// The control's lowest step is silent.
    mute_step: bool,
    /// The level is ours to write (not taken over from the card).
    own_level: bool,
    /// We silenced the control by writing its bottom step.
    at_mute_step: bool,
    /// A mixer failure is logged once, until it works.
    quiet: bool,
    started: bool,
    /// A level was saved, by the settings or by us, since the start.
    saved: bool,
    unsaved: bool,
    changed_at: u64,
}

impl Inner {
    fn software_wanted(&self) -> bool {
        self.wanted.eq_ignore_ascii_case("software")
    }

    fn open_mixer(&mut self) -> bool {
        let mut m = match SimpleMixer::open(&self.device) {
            Ok(m) => m,
            Err(e) => {
                if !self.quiet {
                    log::warn!(
                        "volume: mixer {} unavailable ({e}); software gain",
                        self.device
                    );
                }
                self.quiet = true;
                return false;
            }
        };
        let automatic = self.wanted.eq_ignore_ascii_case("auto");
        let found = if automatic {
            pick_control(&mut m)
        } else {
            m.find(&self.wanted).map(|e| e.name())
        };
        let Some(name) = found else {
            if !self.quiet {
                if automatic {
                    log::warn!(
                        "volume: no known playback control on {}; software gain",
                        self.device
                    );
                } else {
                    log::warn!(
                        "volume: no '{}' playback control on {}; software gain",
                        self.wanted,
                        self.device
                    );
                }
            }
            self.quiet = true;
            return false;
        };
        self.control = name;
        self.mixer = Some(m);
        self.find_control();
        self.quiet = false;
        true
    }

    fn close_mixer(&mut self) {
        self.mixer = None;
        self.has_switch = false;
        self.mute_step = false;
    }

    /// Look the control up by name (after events the old element may be
    /// gone: alsa-lib frees one whose controls were removed) and read
    /// what it can do.
    fn find_control(&mut self) -> bool {
        let Inner { mixer, control, .. } = self;
        let Some(e) = mixer.as_mut().and_then(|m| m.find(control)) else {
            self.has_switch = false;
            self.mute_step = false;
            return false;
        };
        self.has_switch = e.has_switch();
        self.mute_step = e.db_range().is_some_and(|(min, _)| min == DB_GAIN_MUTE);
        true
    }

    /// The current level and mute to the control, or to the software gain.
    /// The gain for the mixer.
    fn apply(&mut self, level: u32, muted: bool) -> i32 {
        if self.mixer.is_some() {
            let events = self.mixer.as_ref().map(|m| m.handle_events());
            if events.is_some_and(|r| r.is_err()) || !self.find_control() {
                // The control gone, or an error: the card went away. Try it
                // afresh.
                self.close_mixer();
                self.open_mixer();
            }
        }
        let silent = muted || level == 0;
        let Inner { mixer, control, .. } = self;
        let Some(e) = mixer.as_mut().and_then(|m| m.find(control)) else {
            return if silent {
                0
            } else {
                fixed::software_gain_q15(level)
            };
        };
        let mut ok = true;
        let mut soft = 32768;
        if self.has_switch {
            ok = e.set_switch(!silent);
        }
        if silent && !self.has_switch {
            if self.mute_step {
                ok = set_level(&e, 0) && ok; // the bottom of its scale is silent
                self.at_mute_step = true;
            } else {
                soft = 0; // the control cannot go silent: the mixer's gain does it
            }
        } else if !silent && (self.own_level || self.at_mute_step) {
            ok = set_level(&e, level) && ok;
            self.at_mute_step = false;
        }
        if !ok {
            log::warn!("volume: cannot set '{}'", self.control);
        }
        soft
    }

    fn changed(&mut self, now_ms: u64) {
        self.unsaved = true;
        self.changed_at = now_ms;
    }
}

pub struct Volume {
    inner: Mutex<Inner>,
    level: AtomicU32,
    muted: AtomicBool,
    reapply: AtomicBool,
    soft_gain_q15: AtomicI32,
    generation: AtomicU32,
    step_size: u32,
}

impl Volume {
    /// Find the control (the `volume-control` setting) and apply the saved
    /// level, or take over the card's current one. A level never set from
    /// here is kept as the card has it and not written back, which could
    /// round it a step up at every start.
    pub fn open(settings: &Audio) -> Volume {
        let device = if settings.volume_device.is_empty() {
            mixer_device_for(&settings.device)
        } else {
            settings.volume_device.clone()
        };
        let mut inner = Inner {
            device,
            wanted: settings.volume_control.clone(),
            control: String::new(),
            mixer: None,
            has_switch: false,
            mute_step: false,
            own_level: false,
            at_mute_step: false,
            quiet: false,
            started: true,
            saved: settings.volume.is_some(),
            unsaved: false,
            changed_at: 0,
        };
        if !inner.software_wanted() {
            inner.open_mixer();
        }
        let level = match settings.volume {
            Some(l) if l <= 100 => {
                inner.own_level = true;
                l
            }
            _ => {
                let Inner { mixer, control, .. } = &mut inner;
                mixer
                    .as_mut()
                    .and_then(|m| m.find(control))
                    .and_then(|e| get_level(&e))
                    .unwrap_or(100)
            }
        };
        let soft = inner.apply(level, false);
        if inner.mixer.is_some() {
            let Inner { mixer, control, .. } = &mut inner;
            let scale = mixer.as_mut().and_then(|m| m.find(control)).map(|e| {
                let switch = if e.has_switch() { ", switch" } else { "" };
                match e.db_range() {
                    Some((mut min, max)) if min < max => {
                        // A scale that bottoms out in mute: show its
                        // quietest real step.
                        if min == DB_GAIN_MUTE
                            && let Some((raw_min, raw_max)) = e.volume_range()
                            && raw_min < raw_max
                            && let Some(db) = e.db_of_volume(raw_min + 1)
                        {
                            min = db;
                        }
                        format!("{}..{} dB{switch}", db_text(min), db_text(max))
                    }
                    _ => format!("no dB scale{switch}"),
                }
            });
            log::info!(
                "volume: '{}' on {} ({}), {level}%",
                inner.control,
                inner.device,
                scale.unwrap_or_default()
            );
        } else {
            log::info!("volume: software gain, {level}%");
        }
        Volume {
            inner: Mutex::new(inner),
            level: AtomicU32::new(level),
            muted: AtomicBool::new(false),
            reapply: AtomicBool::new(false),
            soft_gain_q15: AtomicI32::new(soft),
            generation: AtomicU32::new(1),
            step_size: settings.volume_step.max(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 0..=100.
    pub fn level(&self) -> u32 {
        self.level.load(Ordering::Relaxed)
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// Bumped on every change; the screens re-read `level` and `muted`.
    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::Acquire)
    }

    /// The gain the mixer applies: Q15, 32768 unity, 0 silent. Unity while
    /// a mixer control does the work (0 when that control cannot go silent
    /// by itself).
    pub fn software_gain_q15(&self) -> i32 {
        self.soft_gain_q15.load(Ordering::Relaxed)
    }

    /// The output moved to another card: the control moves with it, and
    /// the level as it is now is written there.
    pub fn rebind(&self, settings: &Audio) {
        let device = if settings.volume_device.is_empty() {
            mixer_device_for(&settings.device)
        } else {
            settings.volume_device.clone()
        };
        let mut inner = self.lock();
        if inner.device == device {
            return;
        }
        log::info!("volume: the control moves to {device}");
        inner.device = device;
        inner.close_mixer();
        inner.control.clear();
        inner.at_mute_step = false;
        inner.quiet = false;
        if !inner.software_wanted() {
            inner.open_mixer();
        }
        inner.own_level = true;
        self.apply_locked(&mut inner);
        self.bump();
    }

    /// The mixer opened the PCM, possibly on a card that was unplugged and
    /// is back at its power-on level. The next `tick` re-applies.
    pub fn output_opened(&self) {
        self.reapply.store(true, Ordering::Release);
    }

    fn apply_locked(&self, inner: &mut Inner) {
        let soft = inner.apply(self.level(), self.muted());
        self.soft_gain_q15.store(soft, Ordering::Relaxed);
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// 0..=100; unmutes.
    pub fn set(&self, level: u32) {
        let level = level.min(100);
        let mut inner = self.lock();
        if !inner.started || (level == self.level() && !self.muted()) {
            return;
        }
        self.level.store(level, Ordering::Relaxed);
        self.muted.store(false, Ordering::Relaxed);
        inner.own_level = true;
        self.apply_locked(&mut inner);
        inner.changed(clock::now_ms());
        self.bump();
    }

    /// Up (positive) or down by the `volume-step` setting. Any volume key
    /// unmutes, as a step up does from a mute key: a driver who turns the
    /// knob wants to hear something.
    pub fn step(&self, direction: i32) {
        let mut inner = self.lock();
        if !inner.started {
            return;
        }
        let now = self.level();
        let level = if direction > 0 {
            (now + self.step_size).min(100)
        } else {
            now.saturating_sub(self.step_size)
        };
        if level == now && !self.muted() {
            return;
        }
        self.level.store(level, Ordering::Relaxed);
        self.muted.store(false, Ordering::Relaxed);
        inner.own_level = true;
        self.apply_locked(&mut inner);
        inner.changed(clock::now_ms());
        self.bump();
    }

    pub fn toggle_mute(&self) {
        let mut inner = self.lock();
        if !inner.started {
            return;
        }
        let muted = !self.muted();
        self.muted.store(muted, Ordering::Relaxed);
        self.apply_locked(&mut inner);
        // Mute itself is not saved: a head unit that starts silent looks
        // broken. A level taken over from the card is, before its first
        // mute: should the program die muted on a control muted by its
        // bottom step, the next start must not take that step over as the
        // level.
        if muted && !inner.saved && !inner.unsaved {
            inner.changed(clock::now_ms());
        }
        self.bump();
    }

    /// From a loop: re-applies after `output_opened`, and hands out the
    /// level to save once it has been left alone for 2 s. The caller
    /// persists it (`audio.volume`).
    pub fn tick(&self) -> Option<u32> {
        self.tick_at(clock::now_ms())
    }

    pub(crate) fn tick_at(&self, now_ms: u64) -> Option<u32> {
        if self.reapply.swap(false, Ordering::AcqRel) {
            // The output was (re)opened: a card that came back gets the
            // level, one that was missing gets another try.
            let mut inner = self.lock();
            if inner.started {
                if inner.mixer.is_none() && !inner.software_wanted() {
                    inner.open_mixer();
                }
                self.apply_locked(&mut inner);
            }
        }
        let mut inner = self.lock();
        if !inner.unsaved || now_ms < inner.changed_at + SAVE_AFTER_MS {
            return None;
        }
        inner.unsaved = false;
        inner.saved = true;
        Some(self.level())
    }

    /// Give the card its level back (mute does not carry across runs) and
    /// close the mixer. The level to save, if one was not saved yet.
    pub fn close(&self) -> Option<u32> {
        let mut inner = self.lock();
        if !inner.started {
            return None;
        }
        inner.started = false;
        if self.muted() && inner.mixer.is_some() {
            self.muted.store(false, Ordering::Relaxed);
            self.apply_locked(&mut inner);
        }
        let save = inner.unsaved.then(|| self.level());
        inner.unsaved = false;
        inner.close_mixer();
        save
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn software(level: Option<u32>) -> Volume {
        Volume::open(&Audio {
            volume: level,
            volume_control: "software".into(),
            volume_step: 5,
            ..Audio::default()
        })
    }

    #[test]
    fn the_control_device_of_a_pcm_name() {
        assert_eq!(mixer_device_for("hw:0,0"), "hw:0");
        assert_eq!(mixer_device_for("plughw:1"), "hw:1");
        assert_eq!(
            mixer_device_for("front:CARD=Device,DEV=0"),
            "hw:CARD=Device"
        );
        assert_eq!(mixer_device_for("plug:hw:1"), "hw:1");
        assert_eq!(mixer_device_for("plug:\"hw:1,0\""), "hw:1");
        assert_eq!(mixer_device_for("pulse"), "default");
        assert_eq!(mixer_device_for("pulse:foo"), "default");
        assert_eq!(mixer_device_for("hw:"), "default");
        assert_eq!(mixer_device_for("default:CARD="), "default");
    }

    #[test]
    fn steps_clamp_and_unmute() {
        let v = software(Some(50));
        assert_eq!(v.level(), 50);
        assert_eq!(v.software_gain_q15(), fixed::software_gain_q15(50));
        let g = v.generation();
        v.step(1);
        assert_eq!(v.level(), 55);
        assert!(v.generation() > g);
        v.set(98);
        v.step(1);
        assert_eq!(v.level(), 100);
        let g = v.generation();
        v.step(1);
        assert_eq!(v.generation(), g, "nothing changed at the top");
        v.set(3);
        v.step(-1);
        assert_eq!(v.level(), 0);
        assert_eq!(v.software_gain_q15(), 0);
        v.toggle_mute();
        assert!(v.muted());
        v.step(1);
        assert!(!v.muted(), "a step up unmutes");
        assert_eq!(v.level(), 5);
        v.toggle_mute();
        assert!(v.muted());
        assert_eq!(v.software_gain_q15(), 0);
        v.toggle_mute();
        assert!(!v.muted());
        assert_eq!(v.software_gain_q15(), fixed::software_gain_q15(5));
    }

    #[test]
    fn the_save_is_debounced() {
        let v = software(Some(50));
        assert_eq!(v.tick_at(10_000), None);
        v.step(1);
        let changed = clock::now_ms();
        assert_eq!(v.tick_at(changed + 100), None);
        v.step(1);
        let changed = clock::now_ms();
        assert_eq!(v.tick_at(changed + 1999), None);
        assert_eq!(v.tick_at(changed + 2000), Some(60));
        assert_eq!(v.tick_at(changed + 5000), None, "saved once");
        v.step(-1);
        assert_eq!(v.close(), Some(55), "closing hands out what is unsaved");
        assert_eq!(v.close(), None);
        v.step(1);
        assert_eq!(v.level(), 55, "closed: keys do nothing");
    }

    #[test]
    fn a_taken_over_level_is_saved_on_the_first_mute() {
        // No control on this host: the level falls back to 100, and it is
        // not "ours" until a key.
        let v = software(None);
        assert_eq!(v.level(), 100);
        assert!(!v.lock().own_level);
        v.toggle_mute();
        let now = clock::now_ms();
        assert_eq!(v.tick_at(now + 2000), Some(100));
        v.toggle_mute();
        v.toggle_mute();
        assert_eq!(v.tick_at(now + 9000), None, "mute itself is never saved");
    }

    #[test]
    fn db_text_and_reapply() {
        assert_eq!(db_text(-6250), "-62.5");
        assert_eq!(db_text(0), "0.0");
        assert_eq!(db_text(1234), "12.3");
        let v = software(Some(40));
        v.output_opened();
        assert_eq!(v.tick_at(1), None);
        assert_eq!(v.software_gain_q15(), fixed::software_gain_q15(40));
    }
}
