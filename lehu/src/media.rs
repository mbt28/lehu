// SPDX-License-Identifier: GPL-3.0-or-later
//! Now playing and the route guidance as the screens show them: the
//! session's side channel turned into the Media page's fields and the
//! navigation card's words. Integers only; runs when the phone sent
//! something new, a few times a second at most.

use std::fmt::Write as _;

use lehu_session::{Guidance, Maneuver, NowPlaying, Playback};
use lehu_ui::{Media, NavArrow, NavCard};

/// The Media page's fields from what the phone plays.
pub fn media_of(np: &NowPlaying, out: &mut Media) {
    out.available = !np.is_empty();
    out.title.clone_from(&np.title);
    out.artist.clone_from(&np.artist);
    out.album.clone_from(&np.album);
    out.duration_ms = np.duration_ms;
    out.position_ms = np.position_ms;
    out.position_at = np.position_at;
    out.playing = np.playback == Playback::Playing;
}

/// The card from the guidance: `zone_min` is the phone's offset from UTC,
/// when it told it, for the arrival as a time of day.
pub fn card_of(g: &Guidance, zone_min: Option<i32>, out: &mut NavCard) {
    out.active = g.active;
    out.arrow = arrow_of(g.maneuver);
    out.maneuver.clear();
    maneuver_words(g.maneuver, &mut out.maneuver);
    out.distance.clear();
    if let Some(m) = g.distance_m {
        distance_words(m, &mut out.distance);
    }
    out.road.clone_from(&g.road);
    out.eta.clear();
    out.eta_note.clear();
    out.eta_note
        .push_str(eta_words(g, zone_min, &mut out.eta).unwrap_or(""));
}

/// The arrow the card draws: the side and how sharp, the rest left to the
/// words (an exit with no side, a ferry).
pub fn arrow_of(m: Maneuver) -> NavArrow {
    match m {
        Maneuver::None | Maneuver::Exit | Maneuver::Ferry => NavArrow::None,
        Maneuver::Depart | Maneuver::Straight | Maneuver::Merge => NavArrow::Straight,
        Maneuver::Left => NavArrow::Left,
        Maneuver::Right => NavArrow::Right,
        Maneuver::SlightLeft | Maneuver::KeepLeft | Maneuver::ExitLeft => NavArrow::SlightLeft,
        Maneuver::SlightRight | Maneuver::KeepRight | Maneuver::ExitRight => NavArrow::SlightRight,
        Maneuver::SharpLeft => NavArrow::SharpLeft,
        Maneuver::SharpRight => NavArrow::SharpRight,
        Maneuver::UTurn => NavArrow::UTurn,
        Maneuver::Roundabout(_) => NavArrow::Roundabout,
        Maneuver::Arrive => NavArrow::Arrive,
    }
}

/// The manoeuvre in a word or three, short enough for the card.
pub fn maneuver_words(m: Maneuver, out: &mut String) {
    let word = match m {
        Maneuver::None => "",
        Maneuver::Depart => "Start",
        Maneuver::Straight => "Straight on",
        Maneuver::Left => "Turn left",
        Maneuver::Right => "Turn right",
        Maneuver::SlightLeft => "Bear left",
        Maneuver::SlightRight => "Bear right",
        Maneuver::SharpLeft => "Sharp left",
        Maneuver::SharpRight => "Sharp right",
        Maneuver::UTurn => "U-turn",
        Maneuver::KeepLeft => "Keep left",
        Maneuver::KeepRight => "Keep right",
        Maneuver::Exit => "Exit",
        Maneuver::ExitLeft => "Exit left",
        Maneuver::ExitRight => "Exit right",
        Maneuver::Merge => "Merge",
        Maneuver::Roundabout(0) => "Roundabout",
        Maneuver::Roundabout(n) => {
            let _ = write!(out, "Roundabout, exit {n}");
            return;
        }
        Maneuver::Ferry => "Ferry",
        Maneuver::Arrive => "Arrive",
    };
    out.push_str(word);
}

/// Metres as a driver reads them: "50 m" and "350 m" in steps of ten,
/// "1.2 km" to ten kilometres, "14 km" past that.
pub fn distance_words(m: u32, out: &mut String) {
    let m = u64::from(m);
    let tens = (m + 5) / 10 * 10;
    if tens < 1000 {
        let _ = write!(out, "{tens} m");
    } else if m < 9950 {
        let tenths = (m + 50) / 100;
        let _ = write!(out, "{}.{} km", tenths / 10, tenths % 10);
    } else {
        let _ = write!(out, "{} km", (m + 500) / 1000);
    }
}

/// The arrival: the phone's own words, else the time of day it gave with
/// its time zone, else the time left -- and the note the card puts under
/// it ("arrival", "to go"). Nothing when none is known.
pub fn eta_words(g: &Guidance, zone_min: Option<i32>, out: &mut String) -> Option<&'static str> {
    if !g.eta_text.is_empty() {
        out.push_str(&g.eta_text);
        return Some("arrival");
    }
    if g.eta_unix > 0
        && let Some(zone) = zone_min
    {
        let local = (g.eta_unix + i64::from(zone) * 60).rem_euclid(86_400);
        let _ = write!(out, "{}:{:02}", local / 3600, local % 3600 / 60);
        return Some("arrival");
    }
    let s = g.remaining_s?;
    let min = (s + 30) / 60;
    if min >= 60 {
        let _ = write!(out, "{} h {:02} min", min / 60, min % 60);
    } else {
        let _ = write!(out, "{min} min");
    }
    Some("to go")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(f: impl Fn(&mut String)) -> String {
        let mut s = String::new();
        f(&mut s);
        s
    }

    #[test]
    fn distances_read_as_on_a_road_sign() {
        let d = |m| words(|s| distance_words(m, s));
        assert_eq!(d(0), "0 m");
        assert_eq!(d(44), "40 m");
        assert_eq!(d(345), "350 m");
        assert_eq!(d(994), "990 m");
        assert_eq!(d(995), "1.0 km");
        assert_eq!(d(1249), "1.2 km");
        assert_eq!(d(1250), "1.3 km");
        assert_eq!(d(9949), "9.9 km");
        assert_eq!(d(9950), "10 km");
        assert_eq!(d(14_400), "14 km");
        assert_eq!(d(u32::MAX), "4294967 km");
    }

    fn eta(g: &Guidance, zone: Option<i32>) -> (String, Option<&'static str>) {
        let mut s = String::new();
        let note = eta_words(g, zone, &mut s);
        (s, note)
    }

    #[test]
    fn the_arrival_prefers_the_phones_words_then_its_clock() {
        let mut g = Guidance {
            eta_unix: 1_790_000_000 + 13 * 3600, // some day, 13:xx UTC
            remaining_s: Some(725),
            ..Guidance::default()
        };
        let base = (1_790_000_000i64 + 13 * 3600).rem_euclid(86_400);
        let (h, m) = (base / 3600, base % 3600 / 60);
        assert_eq!(
            eta(&g, Some(60)),
            (format!("{}:{m:02}", (h + 1) % 24), Some("arrival"))
        );
        // No zone: the time left instead, never a wrong time of day.
        assert_eq!(eta(&g, None), ("12 min".into(), Some("to go")));
        g.remaining_s = Some(3 * 3600 + 5 * 60);
        assert_eq!(eta(&g, None), ("3 h 05 min".into(), Some("to go")));
        g.eta_text = "14:32".into();
        assert_eq!(eta(&g, Some(60)), ("14:32".into(), Some("arrival")));
        assert_eq!(eta(&Guidance::default(), None), (String::new(), None));
    }

    #[test]
    fn the_card_and_the_page_from_the_side_channel() {
        let g = Guidance {
            active: true,
            maneuver: Maneuver::Roundabout(2),
            distance_m: Some(350),
            road: "Main St".into(),
            remaining_s: Some(600),
            ..Guidance::default()
        };
        let mut card = NavCard::default();
        card_of(&g, None, &mut card);
        assert!(card.active);
        assert_eq!(card.arrow, NavArrow::Roundabout);
        assert_eq!(card.maneuver, "Roundabout, exit 2");
        assert_eq!(card.distance, "350 m");
        assert_eq!(card.road, "Main St");
        assert_eq!(
            (card.eta.as_str(), card.eta_note.as_str()),
            ("10 min", "to go")
        );
        card_of(&Guidance::default(), None, &mut card);
        assert_eq!(card, NavCard::default());

        let mut np = NowPlaying::default();
        let mut media = Media::default();
        media_of(&np, &mut media);
        assert!(!media.available);
        np.title.push_str("Song");
        np.set_playback(Playback::Playing, 5);
        media_of(&np, &mut media);
        assert!(media.available && media.playing);
        assert_eq!(media.title, "Song");
        assert_eq!(media.position_at, 5);
    }
}
