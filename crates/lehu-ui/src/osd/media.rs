// SPDX-License-Identifier: GPL-3.0-or-later
//! Now playing and the navigation card. The Home tile and the Media page
//! (screen 11) show what the host says the phone plays; the position runs
//! on from the last position the phone gave, redrawn once a second while
//! the page is up (the Home tile's bar every few seconds). The album art is
//! set once per change, already at its shown size: the page never decodes
//! or scales it, and the position's redraw leaves it alone. The card shows
//! for `NAV_CARD_MS` after each guidance update while the phone guides,
//! over the screens or as the window over the camera's picture, never over
//! the phone's own video (which shows the route itself) and never with the
//! camera column up or a dialog open.

use slint::SharedString;

use super::{Bridge, CAMERA, HOME, Host, MEDIA, NAV_CARD_MS, State, UPDATE_PHONE, clean};
use crate::host::{Media, MediaKey};

/// The Home tile's bar moves on this often while something plays: a thin
/// bar a glance does not follow, not worth a redraw a second.
const HOME_PROGRESS_S: u32 = 5;

impl<H: Host> State<H> {
    /// The host's now playing -> the Home row and the page, on change; the
    /// position line every tick the page is up, written when its second
    /// changes.
    pub(super) fn pump_media(&mut self, bridge: &Bridge<'_>, screen: i32, now: u64) {
        if let Some(art) = self.host.take_art() {
            match art.into_image() {
                Some(image) => {
                    bridge.set_media_art(image);
                    bridge.set_media_art_shown(true);
                }
                None => {
                    bridge.set_media_art_shown(false);
                    bridge.set_media_art(slint::Image::default());
                }
            }
        }
        let generation = self.host.media_generation();
        if !self.media_seen || generation != self.p_media_gen {
            self.media_seen = true;
            self.p_media_gen = generation;
            let m = self.host.media();
            bridge.set_media_title(clean(&mut self.scratch, &m.title));
            bridge.set_media_artist(clean(&mut self.scratch, &m.artist));
            bridge.set_media_album(clean(&mut self.scratch, &m.album));
            let total = if m.available && m.duration_ms > 0 {
                clock(&mut self.scratch, m.duration_ms / 1000)
            } else {
                ""
            };
            bridge.set_media_total(SharedString::from(total));
            let (available, playing) = (m.available, m.playing);
            bridge.set_media_playing(playing);
            if available != self.p_media_available {
                self.p_media_available = available;
                bridge.set_media_available(available);
                if !available && bridge.get_ring_row().as_str() == "media" {
                    self.ring_gone(bridge);
                }
            }
            self.p_media_second = None;
        }
        // The position: on the page each second, on Home every few.
        let every = match screen {
            MEDIA => 1,
            HOME => HOME_PROGRESS_S,
            _ => return,
        };
        let m = self.host.media();
        let at = position(m, now);
        let second = at / 1000 / every * every;
        if self.p_media_second == Some(second) {
            return;
        }
        self.p_media_second = Some(second);
        if screen == MEDIA {
            let elapsed = if m.available {
                clock(&mut self.scratch, at / 1000)
            } else {
                ""
            };
            bridge.set_media_elapsed(SharedString::from(elapsed));
        }
        let progress = if m.available && m.duration_ms > 0 {
            (u64::from(second) * 1_000_000 / u64::from(m.duration_ms)).min(1000) as i32
        } else {
            -1
        };
        if progress != self.p_media_progress {
            self.p_media_progress = progress;
            bridge.set_media_progress(if progress < 0 {
                -1.0
            } else {
                progress as f32 / 1000.0
            });
        }
    }

    /// The host's guidance -> the card's words, on change; whether it shows,
    /// every tick.
    pub(super) fn pump_nav(&mut self, bridge: &Bridge<'_>, screen: i32, modal: bool, now: u64) {
        let generation = self.host.nav_generation();
        if !self.nav_seen || generation != self.p_nav_gen {
            self.nav_seen = true;
            self.p_nav_gen = generation;
            let card = self.host.nav_card();
            bridge.set_nav_arrow(card.arrow.id());
            bridge.set_nav_maneuver(clean(&mut self.scratch, &card.maneuver));
            bridge.set_nav_distance(clean(&mut self.scratch, &card.distance));
            bridge.set_nav_road(clean(&mut self.scratch, &card.road));
            bridge.set_nav_eta(clean(&mut self.scratch, &card.eta));
            bridge.set_nav_eta_note(clean(&mut self.scratch, &card.eta_note));
            self.nav_until = if card.active { now + NAV_CARD_MS } else { 0 };
        }
        let show = now < self.nav_until
            && !self.over_phone
            && !modal
            && !(screen == CAMERA && self.panel_up)
            // The QR codes and what they say need the whole screen.
            && screen != UPDATE_PHONE;
        if show != self.p_nav_card {
            self.p_nav_card = show;
            bridge.set_nav_card(show);
        }
    }

    /// A transport key from the page.
    pub(super) fn media_key(&mut self, key: i32) {
        let key = match key {
            0 => MediaKey::Previous,
            1 => MediaKey::PlayPause,
            2 => MediaKey::Next,
            _ => return,
        };
        self.host.media_key(key);
    }

    /// When the position shown next moves on, if a screen shows one that
    /// runs: the page each second, the Home tile's bar every few.
    pub(super) fn media_due(&self, now: u64) -> Option<u64> {
        let every = match self.p_screen {
            MEDIA => 1000,
            HOME => u64::from(HOME_PROGRESS_S) * 1000,
            _ => return None,
        };
        if self.over_phone {
            return None;
        }
        let m = self.host.media();
        if !m.available || !m.playing {
            return None;
        }
        let at = u64::from(position(m, now));
        Some(now + every - at % every)
    }
}

/// Where the item is at `now`: running on while it plays, never past its
/// end.
pub(super) fn position(m: &Media, now: u64) -> u32 {
    let mut at = u64::from(m.position_ms);
    if m.playing {
        at += now.saturating_sub(m.position_at);
    }
    if m.duration_ms > 0 {
        at = at.min(u64::from(m.duration_ms));
    }
    at.min(u64::from(u32::MAX)) as u32
}

/// Seconds as a player shows them, into `out`: "3:07", "1:02:07".
fn clock(out: &mut String, s: u32) -> &str {
    use std::fmt::Write as _;
    out.clear();
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    let _ = if h > 0 {
        write!(out, "{h}:{m:02}:{s:02}")
    } else {
        write!(out, "{m}:{s:02}")
    };
    out.as_str()
}

#[cfg(test)]
pub(super) fn clock_text(s: u32) -> String {
    let mut out = String::new();
    clock(&mut out, s);
    out
}
