//! The phones the head unit knows: one record per phone, however it
//! came (a cable, Bluetooth, Wi-Fi), with the identities each link gave
//! it, so a phone seen over Bluetooth and later plugged in is one entry,
//! not two. The loop updates it from the sessions and the links; the
//! screens list it as Active (the session), Available (on a link now,
//! ready to start) or Offline. Kept in `devices.toml` beside the user's
//! settings, written only when something changed.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Which protocol a phone speaks: a phone is one or the other for good.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    CarPlay,
    AndroidAuto,
}

/// Where the phone is right now, as the screens show it. Not saved: it
/// follows the links and the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Presence {
    /// Nothing says it is near.
    #[default]
    Offline,
    /// On a link (Bluetooth connected, USB plugged) without a session.
    Available,
    /// The session runs on it.
    Active,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", default)]
pub struct Known {
    /// The protocol's own id: CarPlay's pairing/controller id, Android
    /// Auto's device id. The key a session is matched by.
    pub id: String,
    pub kind: Kind,
    /// What the phone calls itself.
    pub name: String,
    /// Its Bluetooth address, "AA:BB:CC:DD:EE:FF", once seen.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub bt_address: String,
    /// Its USB serial (CarPlay: the UDID without the dash), once seen.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub usb_serial: String,
    /// Seconds since the epoch of its last session, 0 never.
    pub last_seen: u64,
    /// Stopped from the screens: not taken again by itself until it is
    /// picked (Connect) or its cable is plugged again.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub held: bool,
    #[serde(skip)]
    pub presence: Presence,
}

impl Default for Known {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: Kind::CarPlay,
            name: String::new(),
            bt_address: String::new(),
            usb_serial: String::new(),
            last_seen: 0,
            held: false,
            presence: Presence::Offline,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "kebab-case", default)]
struct File {
    devices: Vec<Known>,
}

/// The registry: the records and their file.
#[derive(Debug)]
pub struct Registry {
    path: PathBuf,
    devices: Vec<Known>,
    dirty: bool,
    /// Bumped on every change the screens could see. It starts at 1, so the
    /// phones read at start differ from a screen that has copied none.
    generation: u32,
}

/// At most this many phones are remembered; the one unseen longest goes.
pub const MAX_KNOWN: usize = 8;

impl Registry {
    /// Load `devices.toml` from `dir`, or start empty; a damaged file is
    /// logged and replaced at the next save.
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("devices.toml");
        let devices = match fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<File>(&text) {
                Ok(f) => f.devices,
                Err(e) => {
                    log::warn!(
                        "devices: {} unreadable ({e}); starting over",
                        path.display()
                    );
                    Vec::new()
                }
            },
            Err(_) => Vec::new(),
        };
        // A file written elsewhere may hold more than the registry keeps:
        // the ones seen most recently stay.
        let mut devices = devices;
        if devices.len() > MAX_KNOWN {
            devices.sort_by_key(|d| std::cmp::Reverse(d.last_seen));
            devices.truncate(MAX_KNOWN);
        }
        Self {
            path,
            devices,
            dirty: false,
            generation: 1,
        }
    }

    /// An empty registry that saves nowhere (tests, a read-only image).
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            devices: Vec::new(),
            dirty: false,
            generation: 1,
        }
    }

    pub fn all(&self) -> &[Known] {
        &self.devices
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn by_id(&self, id: &str) -> Option<&Known> {
        self.devices.iter().find(|d| d.id == id)
    }

    pub fn by_bt(&self, address: &str) -> Option<&Known> {
        self.devices
            .iter()
            .find(|d| !d.bt_address.is_empty() && d.bt_address.eq_ignore_ascii_case(address))
    }

    pub fn by_usb(&self, serial: &str) -> Option<&Known> {
        self.devices
            .iter()
            .find(|d| !d.usb_serial.is_empty() && d.usb_serial == serial)
    }

    fn changed(&mut self) {
        self.dirty = true;
        self.generation = self.generation.wrapping_add(1);
    }

    /// A session identified a phone: the record is made or updated with
    /// what this link told about it (an empty identity leaves the stored
    /// one). The phone becomes Active.
    pub fn seen(
        &mut self,
        id: &str,
        kind: Kind,
        name: &str,
        bt_address: &str,
        usb_serial: &str,
        now_secs: u64,
    ) {
        if id.is_empty() {
            return;
        }
        for d in &mut self.devices {
            d.presence = match d.presence {
                Presence::Active => Presence::Offline,
                other => other,
            };
        }
        let at = match self.devices.iter().position(|d| d.id == id) {
            Some(at) => at,
            None => {
                if self.devices.len() >= MAX_KNOWN
                    && let Some(oldest) = self
                        .devices
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, d)| d.last_seen)
                        .map(|(i, _)| i)
                {
                    self.devices.remove(oldest);
                }
                self.devices.push(Known {
                    id: id.to_string(),
                    kind,
                    ..Known::default()
                });
                self.devices.len() - 1
            }
        };
        let d = &mut self.devices[at];
        d.kind = kind;
        if !name.is_empty() {
            d.name = name.to_string();
        }
        if !bt_address.is_empty() {
            d.bt_address = bt_address.to_uppercase();
        }
        if !usb_serial.is_empty() {
            d.usb_serial = usb_serial.to_string();
        }
        d.last_seen = now_secs;
        d.presence = Presence::Active;
        self.changed();
    }

    /// The session ended: nothing is Active. The phone stays Available
    /// while a link still shows it.
    pub fn session_ended(&mut self) {
        for d in &mut self.devices {
            if d.presence == Presence::Active {
                d.presence = Presence::Available;
                self.generation = self.generation.wrapping_add(1);
            }
        }
    }

    /// A link saw the phone with this Bluetooth address, or lost it.
    pub fn bt_present(&mut self, address: &str, present: bool) {
        self.set_presence(|d| d.bt_address.eq_ignore_ascii_case(address), present);
    }

    /// A link saw the phone with this USB serial, or lost it.
    pub fn usb_present(&mut self, serial: &str, present: bool) {
        self.set_presence(|d| d.usb_serial == serial, present);
    }

    fn set_presence(&mut self, matches: impl Fn(&Known) -> bool, present: bool) {
        for d in &mut self.devices {
            if !matches(d) || d.presence == Presence::Active {
                continue;
            }
            let want = if present {
                Presence::Available
            } else {
                Presence::Offline
            };
            if d.presence != want {
                d.presence = want;
                self.generation = self.generation.wrapping_add(1);
            }
        }
    }

    /// Hold a phone off, or let it back; true when that changed anything.
    pub fn hold(&mut self, id: &str, held: bool) -> bool {
        let Some(d) = self.devices.iter_mut().find(|d| d.id == id) else {
            return false;
        };
        if d.held == held {
            return false;
        }
        d.held = held;
        self.changed();
        true
    }

    pub fn is_held(&self, id: &str) -> bool {
        self.by_id(id).is_some_and(|d| d.held)
    }

    /// Forget a phone: its record goes; the caller unpairs it on the links
    /// (the CarPlay store, BlueZ).
    pub fn forget(&mut self, id: &str) -> Option<Known> {
        let at = self.devices.iter().position(|d| d.id == id)?;
        let gone = self.devices.remove(at);
        self.changed();
        Some(gone)
    }

    /// Write the file if anything was saved-worthy since the last write.
    pub fn save_if_dirty(&mut self) -> io::Result<bool> {
        if !self.dirty || self.path.as_os_str().is_empty() {
            self.dirty = false;
            return Ok(false);
        }
        let file = File {
            devices: self.devices.clone(),
        };
        let text = toml::to_string(&file).map_err(io::Error::other)?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("toml.new");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &self.path)?;
        self.dirty = false;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_phone_over_two_links_is_one_record() {
        let mut r = Registry::in_memory();
        r.seen(
            "ctl-1",
            Kind::CarPlay,
            "Alex's iPhone",
            "",
            "0000815000ABC",
            100,
        );
        r.seen("ctl-1", Kind::CarPlay, "", "aa:bb:cc:dd:ee:ff", "", 200);
        assert_eq!(r.all().len(), 1);
        let d = r.by_id("ctl-1").unwrap();
        assert_eq!(d.name, "Alex's iPhone");
        assert_eq!(d.bt_address, "AA:BB:CC:DD:EE:FF");
        assert_eq!(d.usb_serial, "0000815000ABC");
        assert_eq!(d.last_seen, 200);
        assert_eq!(d.presence, Presence::Active);
        assert!(r.by_bt("AA:BB:CC:DD:EE:FF").is_some());
        assert!(r.by_usb("0000815000ABC").is_some());
    }

    #[test]
    fn presence_follows_the_session_and_the_links() {
        let mut r = Registry::in_memory();
        r.seen("a", Kind::CarPlay, "A", "11:11:11:11:11:11", "", 1);
        r.seen("b", Kind::AndroidAuto, "B", "", "SERIAL-B", 2);
        assert_eq!(r.by_id("a").unwrap().presence, Presence::Offline);
        assert_eq!(r.by_id("b").unwrap().presence, Presence::Active);
        r.session_ended();
        assert_eq!(r.by_id("b").unwrap().presence, Presence::Available);
        r.usb_present("SERIAL-B", false);
        assert_eq!(r.by_id("b").unwrap().presence, Presence::Offline);
        r.bt_present("11:11:11:11:11:11", true);
        assert_eq!(r.by_id("a").unwrap().presence, Presence::Available);
        let g = r.generation();
        r.bt_present("11:11:11:11:11:11", true);
        assert_eq!(r.generation(), g, "no change, no generation");
    }

    #[test]
    fn a_held_phone_stays_held_across_a_restart() {
        let dir = std::env::temp_dir().join(format!("lehu-devices-held-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut r = Registry::open(&dir);
        r.seen("a", Kind::CarPlay, "A", "11:11:11:11:11:11", "", 1);
        assert!(!r.is_held("a"));
        assert!(r.hold("a", true));
        assert!(!r.hold("a", true), "no change");
        assert!(!r.hold("nobody", true));
        r.save_if_dirty().unwrap();
        let mut again = Registry::open(&dir);
        assert!(again.is_held("a"));
        assert!(again.hold("a", false));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_oldest_makes_room_and_forget_removes() {
        let mut r = Registry::in_memory();
        for i in 0..MAX_KNOWN + 1 {
            r.seen(
                &format!("id-{i}"),
                Kind::CarPlay,
                "P",
                "",
                "",
                10 + i as u64,
            );
        }
        assert_eq!(r.all().len(), MAX_KNOWN);
        assert!(r.by_id("id-0").is_none(), "the oldest went");
        assert!(r.forget("id-3").is_some());
        assert!(r.forget("id-3").is_none());
        assert_eq!(r.all().len(), MAX_KNOWN - 1);
    }

    #[test]
    fn the_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("lehu-devices-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut r = Registry::open(&dir);
        assert!(r.all().is_empty());
        assert!(!r.save_if_dirty().unwrap(), "nothing to save");
        r.seen(
            "ctl",
            Kind::CarPlay,
            "Phone",
            "AA:BB:CC:DD:EE:FF",
            "SER",
            1234,
        );
        assert!(r.save_if_dirty().unwrap());
        assert!(!r.save_if_dirty().unwrap());
        let again = Registry::open(&dir);
        let d = again.by_id("ctl").unwrap();
        assert_eq!((d.name.as_str(), d.last_seen), ("Phone", 1234));
        assert_eq!(d.presence, Presence::Offline, "presence is not saved");
        fs::write(dir.join("devices.toml"), "not toml [").unwrap();
        assert!(Registry::open(&dir).all().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_screen_that_copied_nothing_sees_the_phones_read_at_start() {
        assert_ne!(Registry::in_memory().generation(), 0);
    }
}
