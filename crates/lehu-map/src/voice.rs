// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Spoken turn prompts: what to say, and when, as a drive nears each turn.
//!
//! Fed every tick with the next turn (an [`Ahead`]) and the speed, [`Voice`]
//! says each turn at most three times, each once: early, about half a
//! minute ahead, as "In 500 metres" or "In 1 kilometre" depending on the
//! speed; near, about ten seconds or 200 metres ahead; and now, two or
//! three seconds before it. A prompt that no longer fits, because the turn
//! comes soon after the last one, is skipped rather than said late or on
//! top of the next; when the turn after this one is that close, it is told
//! with this one ("then turn right") and only its last prompt is said.
//! The destination is "you will reach your destination" ahead, "you have
//! arrived at your destination" there.
//!
//! [`CameraWatch`] says "speed camera ahead" once before each speed camera
//! on a route, when the driver has asked for it.
//!
//! A prompt is a list of [`Clip`]s, each a WAV file the board plays one
//! after the other (scripts/voice-clips.py makes them on a PC). Distances
//! are spoken as a driver hears them: 50 metres steps up to 250, then 100
//! metres steps, then 1, 1.5 and 2 kilometres. Everything is integer
//! arithmetic on the metres and the km/h: it runs on a board without an FPU.

use crate::track::{Ahead, Turn};

/// One clip of a prompt, a WAV file of its own (`<name>.wav`) in the
/// voice's directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clip {
    /// "In 500 metres": a distance as spoken, in metres.
    In(u32),
    /// The turn as said ahead of it: "turn left", "at the roundabout, take
    /// the second exit", "you will reach your destination".
    Do(Turn),
    /// At a roundabout: "take the second exit".
    Exit(u8),
    Now,
    Then,
    Arrived,
    /// "Speed camera ahead": said before a camera on the route, when the
    /// warning is on (map.speed-cameras).
    Camera,
}

impl Clip {
    /// The clip's file name, without the `.wav`.
    pub fn name(self) -> String {
        match self {
            Clip::In(metres) => format!("in-{metres}"),
            Clip::Do(turn) => match turn {
                Turn::Straight => "straight".into(),
                Turn::SlightLeft => "bear-left".into(),
                Turn::Left => "turn-left".into(),
                Turn::SharpLeft => "sharp-left".into(),
                Turn::SlightRight => "bear-right".into(),
                Turn::Right => "turn-right".into(),
                Turn::SharpRight => "sharp-right".into(),
                Turn::UTurn => "u-turn".into(),
                Turn::Roundabout(n @ 1..=MAX_EXIT) => format!("roundabout-{n}"),
                Turn::Roundabout(_) => "roundabout".into(),
                Turn::Arrive => "arrive-ahead".into(),
            },
            Clip::Exit(n @ 1..=MAX_EXIT) => format!("exit-{n}"),
            // An exit past the sixth: "enter the roundabout", the signs say
            // the rest.
            Clip::Exit(_) => "roundabout".into(),
            Clip::Now => "now".into(),
            Clip::Then => "then".into(),
            Clip::Arrived => "arrived".into(),
            Clip::Camera => "speed-camera".into(),
        }
    }
}

/// The exits there are clips for; past it a roundabout is only entered.
pub const MAX_EXIT: u8 = 6;

/// The distances there are clips for ("In 50 metres" ... "In 2
/// kilometres"), in metres.
pub const SPOKEN: [u32; 15] = [
    50, 100, 150, 200, 250, 300, 400, 500, 600, 700, 800, 900, 1000, 1500, 2000,
];

/// A distance as a driver hears it, one of `SPOKEN`: the nearest 50 metres
/// up to 250, the nearest 100 up to 900, then 1, 1.5 or 2 kilometres.
pub fn spoken(metres: u32) -> u32 {
    match metres {
        0..75 => 50,
        75..275 => (metres + 25) / 50 * 50,
        275..950 => (metres + 50) / 100 * 100,
        950..1250 => 1000,
        1250..1750 => 1500,
        _ => 2000,
    }
}

/// A turn's distance changing by more than this from one tick to the next
/// is another turn of the same kind onto the same road (or a drive started
/// over): its prompts are said again.
const NEW_TURN_JUMP_M: u32 = 30;

/// Where the near prompt is said: the first of these at least ten seconds
/// ahead, so what is said is a round distance.
const NEAR_AT: [u32; 6] = [200, 300, 400, 500, 600, 800];
/// Where the early prompt is said: the one nearest half a minute ahead.
const EARLY_AT: [u32; 3] = [500, 1000, 2000];

/// Metres driven in `ms` at `kmh`.
fn travel(kmh: u32, ms: u32) -> u32 {
    (u64::from(kmh) * u64::from(ms) / 3_600) as u32
}

/// How far ahead each prompt is said, at a speed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Leads {
    /// The last prompt: about three seconds ahead.
    now: u32,
    /// "You have arrived": about at the destination.
    arrived: u32,
    near: u32,
    /// None when no round distance is about half a minute ahead (in town
    /// at walking pace, the near prompt is that early already).
    early: Option<u32>,
    /// Room a prompt needs before the next one: one said closer than this
    /// to the next is skipped.
    near_room: u32,
    early_room: u32,
}

impl Leads {
    fn at(kmh: u32) -> Self {
        let near = NEAR_AT
            .into_iter()
            .find(|&m| m >= travel(kmh, 10_000))
            .unwrap_or(NEAR_AT[NEAR_AT.len() - 1]);
        // The ladder's step nearest half a minute ahead, as a ratio: of a
        // and b the one nearer, a below the geometric middle.
        let target = u64::from(travel(kmh, 30_000));
        let mut early = EARLY_AT[0];
        for pair in EARLY_AT.windows(2) {
            if target * target >= u64::from(pair[0]) * u64::from(pair[1]) {
                early = pair[1];
            }
        }
        // Not when it is no earlier than the near one, nor more than 45 s
        // ahead.
        let early = (early >= 2 * near && early <= travel(kmh, 45_000)).then_some(early);
        Self {
            now: travel(kmh, 3_000).clamp(25, 150),
            arrived: travel(kmh, 1_500).clamp(15, 60),
            near,
            early,
            near_room: travel(kmh, 5_000).max(40),
            early_room: travel(kmh, 8_000).max(100),
        }
    }
}

/// How far a turn's prompts have gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Said {
    Nothing,
    Early,
    Near,
    Now,
}

/// The turn the prompts are about.
#[derive(Clone, Debug)]
struct Current {
    turn: Turn,
    road: String,
    metres: u32,
    said: Said,
}

/// The prompts' state along a drive: which turn, and what has been said of
/// it. One per drive; [`Voice::tick`] it with each new position.
#[derive(Clone, Debug, Default)]
pub struct Voice {
    current: Option<Current>,
    /// The next turn was told with this one ("then turn right"): only its
    /// last prompt is left to say.
    chained: Option<Turn>,
}

impl Voice {
    pub fn new() -> Self {
        Self::default()
    }

    /// The prompt to say now, if any, with the next turn `ahead`, the
    /// speed in km/h and the turn after it with the metres between the two
    /// (`Track::then`).
    pub fn tick(
        &mut self,
        ahead: &Ahead<'_>,
        kmh: u32,
        then: Option<(Turn, u32)>,
    ) -> Option<Vec<Clip>> {
        self.step(
            ahead.maneuver.turn,
            &ahead.maneuver.road,
            ahead.metres,
            kmh,
            then,
        )
    }

    /// As `tick`, from the parts: the next turn, onto which road, how far.
    pub fn step(
        &mut self,
        turn: Turn,
        road: &str,
        metres: u32,
        kmh: u32,
        then: Option<(Turn, u32)>,
    ) -> Option<Vec<Clip>> {
        let same = self.current.as_ref().is_some_and(|c| {
            c.turn == turn && c.road == road && metres <= c.metres + NEW_TURN_JUMP_M
        });
        if !same {
            let said = if self.chained.take() == Some(turn) {
                Said::Near
            } else {
                Said::Nothing
            };
            self.current = Some(Current {
                turn,
                road: road.to_string(),
                metres,
                said,
            });
        }
        let leads = Leads::at(kmh);
        let current = self.current.as_mut()?;
        current.metres = metres;
        let now_at = if turn == Turn::Arrive {
            leads.arrived
        } else {
            leads.now
        };
        // The turn after this one, when it comes too soon for prompts of
        // its own.
        let chain = then.filter(|&(next, gap)| {
            turn != Turn::Arrive && next != Turn::Arrive && gap <= leads.near
        });
        let mut said = None;
        if current.said < Said::Now && metres <= now_at {
            current.said = Said::Now;
            said = Some(match turn {
                Turn::Arrive => vec![Clip::Arrived],
                Turn::Roundabout(n) => vec![Clip::Exit(n)],
                _ => vec![Clip::Now, Clip::Do(turn)],
            });
        } else if current.said < Said::Near && metres <= leads.near {
            current.said = Said::Near;
            if metres >= now_at + leads.near_room {
                said = Some(vec![Clip::In(spoken(metres)), Clip::Do(turn)]);
            }
        } else if let Some(early) = leads.early
            && current.said < Said::Early
            && metres <= early
        {
            current.said = Said::Early;
            if metres >= leads.near + leads.early_room {
                // No chain: the near prompt tells the turn after.
                return Some(vec![Clip::In(spoken(metres)), Clip::Do(turn)]);
            }
        }
        let mut clips = said?;
        if let Some((next, _)) = chain {
            clips.extend([Clip::Then, Clip::Do(next)]);
            self.chained = Some(next);
        }
        Some(clips)
    }
}

/// The warning before a speed camera is said this many seconds of driving
/// ahead of it, but no nearer than the first distance and no further than
/// the second: the phrase takes a second and a half, and slowing from 130
/// to 90 km/h at an easy 1.5 m/s² takes seven more and 230 m, so twelve
/// seconds leave room; in town 300 m is still before the junction or two
/// that come first, and past 500 m a warning is forgotten before the
/// camera comes.
const CAMERA_LEAD_S: u32 = 12;
const CAMERA_LEAD_M: (u32, u32) = (300, 500);

/// How far ahead of a speed camera its warning is said, at `kmh`.
pub fn camera_lead(kmh: u32) -> u32 {
    travel(kmh, CAMERA_LEAD_S * 1000).clamp(CAMERA_LEAD_M.0, CAMERA_LEAD_M.1)
}

/// The speed camera warnings along one route: each camera's said once, as
/// it comes within [`camera_lead`].
#[derive(Clone, Debug, Default)]
pub struct CameraWatch {
    /// The last camera warned of, by its index on the route.
    told: Option<usize>,
}

impl CameraWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when "speed camera ahead" is due now, `ms` into the drive at
    /// `kmh`.
    pub fn tick(&mut self, track: &crate::Track, ms: u64, kmh: u32) -> bool {
        let Some((i, _)) = track.camera_ahead(ms, camera_lead(kmh)) else {
            return false;
        };
        if self.told.is_some_and(|t| t >= i) {
            return false;
        }
        self.told = Some(i);
        true
    }
}

/// Every clip a prompt can ask for, by name: what scripts/voice-clips.py
/// must make.
pub fn every_clip() -> Vec<String> {
    let turns = [
        Turn::Straight,
        Turn::SlightLeft,
        Turn::Left,
        Turn::SharpLeft,
        Turn::SlightRight,
        Turn::Right,
        Turn::SharpRight,
        Turn::UTurn,
        Turn::Roundabout(0),
        Turn::Arrive,
    ];
    let mut clips: Vec<Clip> = SPOKEN.into_iter().map(Clip::In).collect();
    clips.extend(turns.into_iter().map(Clip::Do));
    for n in 1..=MAX_EXIT {
        clips.extend([Clip::Do(Turn::Roundabout(n)), Clip::Exit(n)]);
    }
    clips.extend([Clip::Now, Clip::Then, Clip::Arrived, Clip::Camera]);
    clips.into_iter().map(Clip::name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names of a prompt's clips, joined: "in-500 turn-left".
    fn names(clips: &[Clip]) -> String {
        clips.iter().map(|c| c.name()).collect::<Vec<_>>().join(" ")
    }

    /// Drive towards one turn at a steady `kmh` from `from` metres, a tick
    /// every 200 ms: what is said, and how far from the turn.
    fn approach(turn: Turn, kmh: u32, from: u32, then: Option<(Turn, u32)>) -> Vec<(u32, String)> {
        let mut voice = Voice::new();
        let step = travel(kmh, 200).max(1);
        let mut out = Vec::new();
        let mut metres = from;
        loop {
            if let Some(clips) = voice.step(turn, "Rue", metres, kmh, then) {
                out.push((metres, names(&clips)));
            }
            if metres == 0 {
                return out;
            }
            metres = metres.saturating_sub(step);
        }
    }

    #[test]
    fn distances_are_spoken_as_a_driver_hears_them() {
        assert_eq!(spoken(0), 50);
        assert_eq!(spoken(74), 50);
        assert_eq!(spoken(75), 100);
        assert_eq!(spoken(199), 200);
        assert_eq!(spoken(274), 250);
        assert_eq!(spoken(275), 300);
        assert_eq!(spoken(349), 300);
        assert_eq!(spoken(361), 400);
        assert_eq!(spoken(949), 900);
        assert_eq!(spoken(950), 1000);
        assert_eq!(spoken(1300), 1500);
        assert_eq!(spoken(1999), 2000);
        assert_eq!(spoken(9000), 2000);
        for m in (0..3000).step_by(7) {
            assert!(SPOKEN.contains(&spoken(m)), "{m}");
        }
    }

    #[test]
    fn on_the_motorway_one_kilometre_then_400_metres_then_now() {
        let said = approach(Turn::Right, 130, 3000, None);
        let what: Vec<&str> = said.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(
            what,
            ["in-1000 turn-right", "in-400 turn-right", "now turn-right"]
        );
        // Each where it should be: under its distance by less than a tick.
        assert!((993..=1000).contains(&said[0].0), "{said:?}");
        assert!((393..=400).contains(&said[1].0), "{said:?}");
        // Three seconds at 130 km/h.
        assert!((101..=108).contains(&said[2].0), "{said:?}");
    }

    #[test]
    fn in_town_500_metres_then_200_then_now() {
        let said = approach(Turn::Left, 50, 2000, None);
        let what: Vec<&str> = said.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(
            what,
            ["in-500 turn-left", "in-200 turn-left", "now turn-left"]
        );
        assert!((38..=41).contains(&said[2].0), "{said:?}");
        // Slowly, the near prompt is the first: half a minute is no round
        // distance further.
        let said = approach(Turn::SlightLeft, 20, 2000, None);
        let what: Vec<&str> = said.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(what, ["in-200 bear-left", "now bear-left"]);
        // Standing still, the last prompt still comes, 25 m ahead.
        let mut voice = Voice::new();
        assert!(voice.step(Turn::Left, "", 30, 0, None).is_none());
        let now = voice.step(Turn::Left, "", 25, 0, None).unwrap();
        assert_eq!(names(&now), "now turn-left");
    }

    #[test]
    fn nothing_is_said_twice() {
        let mut voice = Voice::new();
        let mut said = 0;
        // A stop at 190 m, the distance jittering by a few metres.
        for metres in [210, 199, 195, 199, 205, 190, 196, 190, 191, 180] {
            if voice.step(Turn::Right, "A", metres, 30, None).is_some() {
                said += 1;
            }
        }
        assert_eq!(said, 1, "the near prompt, once");
        for metres in [24, 20, 26, 10, 0] {
            if let Some(c) = voice.step(Turn::Right, "A", metres, 30, None) {
                assert_eq!(names(&c), "now turn-right");
                said += 1;
            }
        }
        assert_eq!(said, 2);
    }

    #[test]
    fn a_turn_soon_after_the_last_skips_the_prompts_that_no_longer_fit() {
        // Found 450 m ahead at 50 km/h: past the early point, but with room
        // for it before the near one (said rounded, as 500).
        let said = approach(Turn::Left, 50, 450, None);
        let what: Vec<&str> = said.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(
            what,
            ["in-500 turn-left", "in-200 turn-left", "now turn-left"]
        );
        let said = approach(Turn::Left, 50, 280, None);
        let what: Vec<&str> = said.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(
            what,
            ["in-200 turn-left", "now turn-left"],
            "too close for early"
        );
        // 120 m: the near prompt still fits, rounded.
        let said = approach(Turn::Left, 50, 120, None);
        let what: Vec<&str> = said.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(what, ["in-100 turn-left", "now turn-left"]);
        // 90 m: only the last.
        let said = approach(Turn::Left, 50, 90, None);
        let what: Vec<&str> = said.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(what, ["now turn-left"]);
        // Found inside the last prompt's distance: said at once.
        let said = approach(Turn::Left, 50, 30, None);
        assert_eq!(said, [(30, "now turn-left".to_string())]);
    }

    #[test]
    fn roundabouts_say_the_exit() {
        let said = approach(Turn::Roundabout(2), 50, 1000, None);
        let what: Vec<&str> = said.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(
            what,
            ["in-500 roundabout-2", "in-200 roundabout-2", "exit-2"]
        );
        let said = approach(Turn::Roundabout(9), 50, 300, None);
        let what: Vec<&str> = said.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(what, ["in-200 roundabout", "roundabout"]);
    }

    #[test]
    fn the_destination_is_told_ahead_and_on_arrival() {
        let said = approach(Turn::Arrive, 50, 1000, None);
        let what: Vec<&str> = said.iter().map(|(_, s)| s.as_str()).collect();
        assert_eq!(
            what,
            ["in-500 arrive-ahead", "in-200 arrive-ahead", "arrived"]
        );
        // "You have arrived" when there, not three seconds before.
        assert!(said[2].0 <= 20, "{said:?}");
    }

    #[test]
    fn a_turn_close_behind_another_is_told_with_it() {
        let mut voice = Voice::new();
        let then = Some((Turn::Right, 80));
        let mut said = Vec::new();
        for metres in (0..=600).rev().step_by(3) {
            if let Some(c) = voice.step(Turn::Left, "A", metres, 50, then) {
                said.push(names(&c));
            }
        }
        assert_eq!(
            said,
            [
                "in-500 turn-left",
                "in-200 turn-left then turn-right",
                "now turn-left then turn-right"
            ]
        );
        // Past the left, the right's own early and near prompts are not
        // said again: only its last.
        for metres in (0..=80).rev().step_by(3) {
            if let Some(c) = voice.step(Turn::Right, "B", metres, 50, None) {
                said.push(names(&c));
            }
        }
        assert_eq!(said.last().unwrap(), "now turn-right");
        assert_eq!(said.len(), 4);
        // A turn far enough behind has prompts of its own.
        let said = approach(Turn::Left, 50, 600, Some((Turn::Right, 400)));
        assert!(said.iter().all(|(_, s)| !s.contains("then")), "{said:?}");
    }

    #[test]
    fn the_same_turn_again_is_a_new_one() {
        let mut voice = Voice::new();
        assert!(voice.step(Turn::Right, "A", 190, 30, None).is_some());
        assert!(voice.step(Turn::Right, "A", 20, 30, None).is_some());
        assert!(voice.step(Turn::Right, "A", 5, 30, None).is_none());
        // Another right onto the same road, 190 m on: said again.
        let c = voice.step(Turn::Right, "A", 190, 30, None).unwrap();
        assert_eq!(names(&c), "in-200 turn-right");
        // Another road is another turn whatever the distance.
        let c = voice.step(Turn::Right, "B", 20, 30, None).unwrap();
        assert_eq!(names(&c), "now turn-right");
    }

    #[test]
    fn the_clips_are_the_ones_the_script_makes() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/voice-clips.py");
        let script = std::fs::read_to_string(path).unwrap();
        let clips = every_clip();
        assert_eq!(clips.len(), 15 + 10 + 12 + 4);
        for name in clips {
            assert!(
                script.contains(&format!("\"{name}\"")),
                "{name} is not in {path}"
            );
        }
    }

    #[test]
    fn a_replayed_drive_is_guided_turn_by_turn() {
        // North at 50 km/h (1.25e-4 degrees a second), left after a
        // minute, the destination after two.
        let mut gpx = String::from(
            r#"<gpx>
<wpt lat="49.60750" lon="6.13"><name>Rue A</name><type>left</type></wpt>
<wpt lat="49.61500" lon="6.13"><name></name><type>arrive</type></wpt>
<trk><trkseg>
"#,
        );
        for s in 0..=120 {
            let lat = 49.6 + f64::from(s) * 1.25e-4;
            gpx.push_str(&format!(
                "<trkpt lat=\"{lat:.6}\" lon=\"6.13\"><time>2027-01-15T08:{:02}:{:02}Z</time></trkpt>\n",
                s / 60,
                s % 60
            ));
        }
        gpx.push_str("</trkseg></trk></gpx>");
        let track = crate::Track::parse_gpx(&gpx).unwrap();
        let mut voice = Voice::new();
        let mut said = Vec::new();
        // Two laps: the drive starts over at its end.
        for ms in (0..2 * track.duration()).step_by(200) {
            let fix = track.at(ms);
            let ahead = track.ahead(ms).unwrap();
            let then = track.then(&ahead).map(|(m, gap)| (m.turn, gap));
            if let Some(clips) = voice.tick(&ahead, fix.kmh, then) {
                said.push(names(&clips));
            }
        }
        let lap = [
            "in-500 turn-left",
            "in-200 turn-left",
            "now turn-left",
            "in-500 arrive-ahead",
            "in-200 arrive-ahead",
            "arrived",
        ];
        assert_eq!(said, [lap, lap].concat());
        let ahead = track.ahead(0).unwrap();
        let (next, gap) = track.then(&ahead).unwrap();
        assert_eq!(next.turn, Turn::Arrive);
        assert!((825..=845).contains(&gap), "{gap}");
        assert!(track.then(&track.ahead(100_000).unwrap()).is_none());
    }

    #[test]
    fn a_camera_is_warned_of_once_and_earlier_when_fast() {
        assert_eq!(camera_lead(0), 300);
        assert_eq!(camera_lead(50), 300);
        assert_eq!(camera_lead(110), 366);
        assert_eq!(camera_lead(130), 433);
        assert_eq!(camera_lead(200), 500);
        // 3 km straight east at 36 km/h, cameras at 1 and 2 km.
        let p = |east: f64| {
            (
                555_000_000 + (east / crate::graph::build::M_PER_PX) as i64,
                368_000_000,
            )
        };
        let leg = crate::Leg {
            points: vec![p(0.0), p(1500.0), p(3000.0)],
            steps: vec![
                crate::LegStep {
                    cm_s: 1000,
                    limit: Some(50)
                };
                2
            ],
            turns: vec![(2, Turn::Arrive, String::new())],
            cameras: vec![
                crate::LegCamera {
                    step: 0,
                    x: p(1000.0).0,
                    y: p(1000.0).1,
                    limit: Some(50),
                },
                crate::LegCamera {
                    step: 1,
                    x: p(2000.0).0,
                    y: p(2000.0).1,
                    limit: None,
                },
            ],
        };
        let track = crate::Track::from_route(&leg).unwrap();
        let c = track.cameras();
        assert_eq!(c.len(), 2);
        // Where they are along it, as the track measures the ground.
        let m = |east: f64| f64::from(crate::metres_between(p(0.0), p(east)));
        assert!((c[0].metres - m(1000.0)).abs() < 2.0, "{c:?}");
        assert!((c[1].metres - m(2000.0)).abs() < 2.0, "{c:?}");
        assert_eq!(c[0].limit, Some(50));
        let mut watch = CameraWatch::new();
        let mut said = Vec::new();
        for ms in (0..track.duration()).step_by(200) {
            if watch.tick(&track, ms, 36) {
                said.push(track.camera_ahead(ms, 1000).unwrap());
            }
        }
        // Each once, 300 m before it (a tick is 2 m).
        assert_eq!(said.len(), 2, "{said:?}");
        assert!(
            said.iter().all(|&(_, m)| (297..=300).contains(&m)),
            "{said:?}"
        );
        assert_eq!((said[0].0, said[1].0), (0, 1));
        // Past the last, nothing ahead.
        assert_eq!(track.camera_ahead(track.duration(), 5000), None);
    }
}
