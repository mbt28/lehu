// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! What a session tells the loop in words: the phone's now playing, its
//! album art and its route guidance. Strings do not travel in
//! [`Event`](crate::Event) (it is `Copy` and fixed-size so the queues never
//! allocate), so each lives in a [`Latest`] in the ports: the session
//! writes it in place and reports `Event::NowPlaying` or `Event::Guidance`;
//! the loop copies it out when that event arrives. A few updates a second
//! at most, each a few short strings rewritten in place; the art changes
//! with the track.

use std::sync::Mutex;

/// The newest value of something, with a generation that moves on every
/// write. Written by a session's thread, read by the loop.
#[derive(Debug, Default)]
pub struct Latest<T> {
    inner: Mutex<(u32, T)>,
}

impl<T: Clone + Default> Latest<T> {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new((0, T::default())),
        }
    }

    /// Change the value in place; `f` says whether it did, and only then
    /// does the generation move.
    pub fn update(&self, f: impl FnOnce(&mut T) -> bool) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let changed = f(&mut g.1);
        if changed {
            g.0 = g.0.wrapping_add(1);
        }
        changed
    }

    pub fn generation(&self) -> u32 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).0
    }

    /// Copy the value into `out` when its generation is not `seen`; the
    /// generation copied. `clone_from` reuses `out`'s strings.
    pub fn read_into(&self, seen: u32, out: &mut T) -> Option<u32> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.0 == seen {
            return None;
        }
        out.clone_from(&g.1);
        Some(g.0)
    }
}

/// Rewrite `field` with `value`, keeping its allocation.
pub fn set_text(field: &mut String, value: &str) {
    if field != value {
        field.clear();
        field.push_str(value);
    }
}

/// Whether the phone plays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Playback {
    #[default]
    Stopped,
    Playing,
    Paused,
}

/// What the phone plays, as its last update said.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// The app playing it ("Spotify"), when the phone says.
    pub app: String,
    /// 0 when unknown (a stream).
    pub duration_ms: u32,
    /// Where it was at `position_at`, the loop's clock (`lehu_core::clock`).
    pub position_ms: u32,
    pub position_at: u64,
    pub playback: Playback,
}

impl Clone for NowPlaying {
    fn clone(&self) -> Self {
        let mut out = Self::default();
        out.clone_from(self);
        out
    }

    fn clone_from(&mut self, src: &Self) {
        self.title.clone_from(&src.title);
        self.artist.clone_from(&src.artist);
        self.album.clone_from(&src.album);
        self.app.clone_from(&src.app);
        self.duration_ms = src.duration_ms;
        self.position_ms = src.position_ms;
        self.position_at = src.position_at;
        self.playback = src.playback;
    }
}

impl NowPlaying {
    /// Nothing worth a row: no title, nothing playing.
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.playback == Playback::Stopped
    }

    /// Where it is at `now`: running on while it plays, never past its end.
    pub fn position(&self, now: u64) -> u32 {
        let mut at = u64::from(self.position_ms);
        if self.playback == Playback::Playing {
            at += now.saturating_sub(self.position_at);
        }
        if self.duration_ms > 0 {
            at = at.min(u64::from(self.duration_ms));
        }
        at.min(u64::from(u32::MAX)) as u32
    }

    pub fn set_position(&mut self, ms: u32, now: u64) {
        self.position_ms = ms;
        self.position_at = now;
    }

    /// Playing, paused or stopped from `now` on: the position stops where
    /// it got to when the music does.
    pub fn set_playback(&mut self, playback: Playback, now: u64) {
        if playback != self.playback {
            let at = self.position(now);
            self.set_position(at, now);
            self.playback = playback;
        }
    }

    /// A new item: what the last one said of itself goes.
    pub fn new_item(&mut self) {
        self.title.clear();
        self.artist.clear();
        self.album.clear();
        self.duration_ms = 0;
    }

    pub fn clear(&mut self) {
        self.new_item();
        self.app.clear();
        self.position_ms = 0;
        self.position_at = 0;
        self.playback = Playback::Stopped;
    }
}

/// The most album art taken, compressed: art is tens of kilobytes, and
/// more than this is refused as none.
pub const MAX_ARTWORK: usize = 512 * 1024;

/// The album art of what plays, compressed as the phone sent it (JPEG or
/// PNG): decoding it is the loop's business, off its own thread. Empty
/// when there is none.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Artwork {
    pub data: Vec<u8>,
}

impl Clone for Artwork {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
        }
    }

    fn clone_from(&mut self, src: &Self) {
        self.data.clone_from(&src.data);
    }
}

impl Artwork {
    /// Take `data` as the art; false when it is the art already. Art past
    /// `MAX_ARTWORK` counts as none.
    pub fn set(&mut self, data: &[u8]) -> bool {
        let data = if data.len() > MAX_ARTWORK {
            &[][..]
        } else {
            data
        };
        if self.data == data {
            return false;
        }
        self.data.clear();
        self.data.extend_from_slice(data);
        if data.is_empty() {
            self.data = Vec::new();
        }
        true
    }

    /// Take the bytes in `buf` as the art without copying them; `buf` gets
    /// the old art's buffer back. False when it is the art already.
    pub fn replace(&mut self, buf: &mut Vec<u8>) -> bool {
        if buf.len() > MAX_ARTWORK {
            buf.clear();
        }
        if self.data == *buf {
            return false;
        }
        std::mem::swap(&mut self.data, buf);
        true
    }

    /// No art: false when there was none.
    pub fn clear(&mut self) -> bool {
        self.set(&[])
    }
}

/// The next manoeuvre, as few kinds as a word can tell apart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Maneuver {
    #[default]
    None,
    Depart,
    Straight,
    Left,
    Right,
    SlightLeft,
    SlightRight,
    SharpLeft,
    SharpRight,
    UTurn,
    KeepLeft,
    KeepRight,
    /// Leave the motorway; the side when the phone says.
    Exit,
    ExitLeft,
    ExitRight,
    Merge,
    /// The exit to take, 0 when the phone does not say.
    Roundabout(u8),
    Ferry,
    Arrive,
}

/// The phone's route guidance, as its last updates said.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Guidance {
    /// A route is being driven.
    pub active: bool,
    pub maneuver: Maneuver,
    /// To the manoeuvre, in metres.
    pub distance_m: Option<u32>,
    /// The road the manoeuvre leads onto, else the one driven on.
    pub road: String,
    /// The arrival time as the phone words it (Android Auto), else empty.
    pub eta_text: String,
    /// The arrival, Unix seconds (CarPlay); 0 when not known.
    pub eta_unix: i64,
    /// Seconds left to the destination.
    pub remaining_s: Option<u32>,
}

impl Clone for Guidance {
    fn clone(&self) -> Self {
        let mut out = Self::default();
        out.clone_from(self);
        out
    }

    fn clone_from(&mut self, src: &Self) {
        self.active = src.active;
        self.maneuver = src.maneuver;
        self.distance_m = src.distance_m;
        self.road.clone_from(&src.road);
        self.eta_text.clone_from(&src.eta_text);
        self.eta_unix = src.eta_unix;
        self.remaining_s = src.remaining_s;
    }
}

impl Guidance {
    /// Guidance stopped: nothing of the route stays.
    pub fn clear(&mut self) {
        self.active = false;
        self.maneuver = Maneuver::None;
        self.distance_m = None;
        self.road.clear();
        self.eta_text.clear();
        self.eta_unix = 0;
        self.remaining_s = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_moves_its_generation_only_on_a_change() {
        let l: Latest<NowPlaying> = Latest::new();
        let mut copy = NowPlaying::default();
        assert_eq!(l.read_into(0, &mut copy), None);
        assert!(!l.update(|_| false));
        assert_eq!(l.generation(), 0);
        assert!(l.update(|np| {
            set_text(&mut np.title, "Song");
            true
        }));
        let seen = l.read_into(0, &mut copy).expect("a new value");
        assert_eq!(copy.title, "Song");
        assert_eq!(l.read_into(seen, &mut copy), None);
    }

    #[test]
    fn the_position_runs_while_playing_and_stops_at_the_end() {
        let mut np = NowPlaying {
            duration_ms: 10_000,
            ..NowPlaying::default()
        };
        np.set_position(2000, 100);
        assert_eq!(np.position(5100), 2000, "stopped: it stays");
        np.set_playback(Playback::Playing, 100);
        assert_eq!(np.position(1100), 3000);
        assert_eq!(np.position(99_999), 10_000, "never past the end");
        np.set_playback(Playback::Paused, 2100);
        assert_eq!(np.position(9000), 4000, "paused where it got to");
        assert!(!np.is_empty());
        np.clear();
        assert!(np.is_empty());
        assert_eq!(np.position(9000), 0);
    }

    #[test]
    fn artwork_changes_only_when_its_bytes_do() {
        let mut a = Artwork::default();
        assert!(!a.clear(), "none already");
        assert!(a.set(b"\xff\xd8art"));
        assert!(!a.set(b"\xff\xd8art"));
        let mut buf = b"\x89PNG".to_vec();
        assert!(a.replace(&mut buf));
        assert_eq!(
            (a.data.as_slice(), buf.as_slice()),
            (&b"\x89PNG"[..], &b"\xff\xd8art"[..])
        );
        let mut same = b"\x89PNG".to_vec();
        assert!(!a.replace(&mut same));
        // Too much is none.
        assert!(a.set(&vec![1; MAX_ARTWORK + 1]));
        assert!(a.data.is_empty());
        let mut big = vec![1; MAX_ARTWORK + 1];
        assert!(!a.replace(&mut big));
        assert!(a.data.is_empty());
    }

    #[test]
    fn clone_from_keeps_the_strings() {
        let src = Guidance {
            active: true,
            road: "Main St".into(),
            ..Guidance::default()
        };
        let mut out = Guidance::default();
        out.road.reserve(64);
        let cap = out.road.capacity();
        out.clone_from(&src);
        assert_eq!(out, src);
        assert_eq!(out.road.capacity(), cap);
        out.clear();
        assert_eq!(out, Guidance::default());
    }
}
