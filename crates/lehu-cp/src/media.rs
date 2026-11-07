//! Now playing and route guidance over iAP2: the subscriptions the head
//! unit sends once the phone accepted it, and the phone's updates parsed
//! into the session's side channel. The parameter ids, the guidance
//! states and the manoeuvre table follow LIVI's iAP2 messages (GPL-3.0,
//! see NOTICE).
//!
//! The parsers walk the parameters in place and never index past a length
//! the wire gave; a value of the wrong size is skipped, not trusted. An
//! update rewrites a few short strings, a few times a second at most. The
//! album art comes apart from the update, over the link's file transfer
//! session (`crate::file`): the update only names its transfer.

use lehu_session::{Guidance, Maneuver, NowPlaying, Playback, set_text};

use crate::iap2::pack_csm;

pub const MSG_START_NOW_PLAYING: u16 = 0x5000;
pub const MSG_NOW_PLAYING_UPDATE: u16 = 0x5001;
pub const MSG_STOP_NOW_PLAYING: u16 = 0x5002;
pub const MSG_START_ROUTE_GUIDANCE: u16 = 0x5200;
pub const MSG_ROUTE_GUIDANCE_UPDATE: u16 = 0x5201;
pub const MSG_ROUTE_GUIDANCE_MANEUVER: u16 = 0x5202;
pub const MSG_STOP_ROUTE_GUIDANCE: u16 = 0x5203;

// NowPlayingUpdate: two groups.
const NP_MEDIA_ITEM: u16 = 0;
const NP_PLAYBACK: u16 = 1;
// MediaItemAttributes.
const ITEM_TITLE: u16 = 1;
const ITEM_DURATION_MS: u16 = 4;
const ITEM_ALBUM: u16 = 6;
const ITEM_ARTIST: u16 = 12;
/// The id of the file transfer that brings the item's artwork.
const ITEM_ARTWORK: u16 = 26;
// PlaybackAttributes.
const PLAY_STATUS: u16 = 0;
const PLAY_ELAPSED_MS: u16 = 1;
const PLAY_APP_NAME: u16 = 7;

// RouteGuidanceUpdate.
const RG_STATE: u16 = 1;
const RG_CURRENT_ROAD: u16 = 3;
const RG_ETA: u16 = 5;
const RG_TIME_REMAINING: u16 = 6;
const RG_DISTANCE_TO_MANEUVER: u16 = 10;
const RG_CURRENT_MANEUVERS: u16 = 13;
// RouteGuidanceManeuverUpdate.
const MAN_INDEX: u16 = 1;
const MAN_TYPE: u16 = 3;
const MAN_AFTER_ROAD: u16 = 4;

/// StartNowPlayingUpdates: the item's title, artist, album and duration,
/// with `artwork` its art too (over file transfer), and whether it plays,
/// where it is and the app.
pub fn start_now_playing(artwork: bool) -> Vec<u8> {
    let full = [
        ITEM_TITLE,
        ITEM_DURATION_MS,
        ITEM_ALBUM,
        ITEM_ARTIST,
        ITEM_ARTWORK,
    ];
    let item = pack_flags(if artwork { &full } else { &full[..4] });
    let playback = pack_flags(&[PLAY_STATUS, PLAY_ELAPSED_MS, PLAY_APP_NAME]);
    pack_csm(
        MSG_START_NOW_PLAYING,
        &[(NP_MEDIA_ITEM, &item), (NP_PLAYBACK, &playback)],
    )
}

/// StartRouteGuidanceUpdates, as LIVI sends it: no parameters, the one
/// display component the identification declared is meant.
pub fn start_route_guidance() -> Vec<u8> {
    pack_csm(MSG_START_ROUTE_GUIDANCE, &[])
}

/// Flag parameters (present, no value), as the Start messages ask.
fn pack_flags(ids: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ids.len() * 4);
    for id in ids {
        out.extend_from_slice(&4u16.to_be_bytes());
        out.extend_from_slice(&id.to_be_bytes());
    }
    out
}

/// The parameters of a body, in place: `(id, value)` until the body ends or
/// a length does not fit, which ends the walk.
#[derive(Clone, Copy)]
struct Params<'a> {
    body: &'a [u8],
}

impl<'a> Iterator for Params<'a> {
    type Item = (u16, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let head = self.body.get(..4)?;
        let len = usize::from(u16::from_be_bytes([head[0], head[1]]));
        let id = u16::from_be_bytes([head[2], head[3]]);
        if len < 4 || len > self.body.len() {
            self.body = &[];
            return None;
        }
        let value = &self.body[4..len];
        self.body = &self.body[len..];
        Some((id, value))
    }
}

/// The parameters of a CSM whose id is `want`; `None` for another message
/// or a header that does not hold.
fn params_of(csm: &[u8], want: u16) -> Option<Params<'_>> {
    let head = csm.get(..6)?;
    if head[..2] != [0x40, 0x40] || u16::from_be_bytes([head[4], head[5]]) != want {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([head[2], head[3]]));
    let body = csm.get(6..len)?;
    Some(Params { body })
}

fn be_u16(v: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes(v.try_into().ok()?))
}

fn be_u32(v: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(v.try_into().ok()?))
}

fn be_u64(v: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(v.try_into().ok()?))
}

/// An iAP2 string: UTF-8 ended by a NUL; one that is not UTF-8 is lost
/// rather than shown wrong.
fn text(v: &[u8]) -> &str {
    let v = v.strip_suffix(&[0]).unwrap_or(v);
    std::str::from_utf8(v).unwrap_or("")
}

/// What a NowPlayingUpdate said of the item's art.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ItemArt {
    /// The title changed: another item, whose art the old one's is not.
    pub new_item: bool,
    /// The file transfer bringing the item's artwork.
    pub transfer: Option<u8>,
}

/// Apply a NowPlayingUpdate to `np`, `now` being the loop's clock. Only
/// the attributes it carries change, but a media item with a title is a
/// new item: what the old one said of itself goes first. `None` when the
/// message is not one; what it said of the art otherwise.
pub fn now_playing_update(csm: &[u8], np: &mut NowPlaying, now: u64) -> Option<ItemArt> {
    let params = params_of(csm, MSG_NOW_PLAYING_UPDATE)?;
    let mut art = ItemArt::default();
    for (id, group) in params {
        match id {
            NP_MEDIA_ITEM => {
                let fields = Params { body: group };
                if let Some((_, title)) = fields.into_iter().find(|(k, _)| *k == ITEM_TITLE) {
                    art.new_item = text(title) != np.title;
                    np.new_item();
                }
                if let Some((_, [ftid])) = fields.into_iter().find(|(k, _)| *k == ITEM_ARTWORK) {
                    art.transfer = Some(*ftid);
                }
                for (k, v) in fields {
                    match k {
                        ITEM_TITLE => set_text(&mut np.title, text(v)),
                        ITEM_ARTIST => set_text(&mut np.artist, text(v)),
                        ITEM_ALBUM => set_text(&mut np.album, text(v)),
                        ITEM_DURATION_MS => {
                            if let Some(ms) = be_u32(v) {
                                np.duration_ms = ms;
                            }
                        }
                        _ => {}
                    }
                }
            }
            NP_PLAYBACK => {
                // The position first: a status in the same update starts
                // or stops it from there.
                let mut status = None;
                for (k, v) in (Params { body: group }) {
                    match (k, v) {
                        (PLAY_ELAPSED_MS, _) => {
                            if let Some(ms) = be_u32(v) {
                                np.set_position(ms, now);
                            }
                        }
                        (PLAY_STATUS, [s]) => status = Some(*s),
                        (PLAY_APP_NAME, _) => set_text(&mut np.app, text(v)),
                        _ => {}
                    }
                }
                if let Some(s) = status {
                    // Seeking forwards or back still plays.
                    let playback = match s {
                        1 | 3 | 4 => Playback::Playing,
                        2 => Playback::Paused,
                        _ => Playback::Stopped,
                    };
                    np.set_playback(playback, now);
                }
            }
            _ => {}
        }
    }
    Some(art)
}

/// What the manoeuvre updates said, by index: the phone describes each
/// manoeuvre once and then names the current ones by index. Bounded: the
/// identification asks the phone to keep no list for us, so it describes
/// the next one or two; the oldest slot goes when a new index comes.
#[derive(Debug, Default)]
pub struct Route {
    slots: [Slot; ROUTE_SLOTS],
    /// The next manoeuvre's index, once the guidance named it.
    current: Option<u16>,
    /// The road driven on, for when the manoeuvre names none.
    road: String,
    next_slot: usize,
}

const ROUTE_SLOTS: usize = 4;

#[derive(Debug, Default)]
struct Slot {
    index: Option<u16>,
    maneuver: Maneuver,
    road: String,
}

impl Route {
    fn slot(&self, index: u16) -> Option<&Slot> {
        self.slots.iter().find(|s| s.index == Some(index))
    }

    /// The slot for `index`, taking the oldest when it is new.
    fn slot_mut(&mut self, index: u16) -> &mut Slot {
        let at = match self.slots.iter().position(|s| s.index == Some(index)) {
            Some(i) => i,
            None => {
                let i = self.next_slot;
                self.next_slot = (i + 1) % ROUTE_SLOTS;
                let s = &mut self.slots[i];
                s.index = Some(index);
                s.maneuver = Maneuver::None;
                s.road.clear();
                i
            }
        };
        &mut self.slots[at]
    }

    /// The current manoeuvre into `g`, from what is known of it.
    fn apply(&self, g: &mut Guidance) {
        let slot = self.current.and_then(|i| self.slot(i));
        g.maneuver = slot.map_or(Maneuver::None, |s| s.maneuver);
        match slot.filter(|s| !s.road.is_empty()) {
            Some(s) => set_text(&mut g.road, &s.road),
            None => set_text(&mut g.road, &self.road),
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Apply a RouteGuidanceUpdate or a RouteGuidanceManeuverUpdate to `g`.
/// False when the message is neither.
pub fn route_guidance(csm: &[u8], route: &mut Route, g: &mut Guidance) -> bool {
    if let Some(params) = params_of(csm, MSG_ROUTE_GUIDANCE_UPDATE) {
        for (k, v) in params {
            match k {
                RG_STATE => {
                    if let [s] = v {
                        // Set, loading, locating, rerouting and proceeding
                        // to the route guide; no route and arrived do not.
                        g.active = matches!(*s, 1 | 3 | 4 | 5 | 6);
                    }
                }
                RG_CURRENT_ROAD => set_text(&mut route.road, text(v)),
                RG_ETA => {
                    if let Some(t) = be_u64(v) {
                        g.eta_unix = i64::try_from(t).unwrap_or(0);
                    }
                }
                RG_TIME_REMAINING => {
                    if let Some(t) = be_u64(v) {
                        g.remaining_s = Some(u32::try_from(t).unwrap_or(u32::MAX));
                    }
                }
                RG_DISTANCE_TO_MANEUVER => {
                    if let Some(m) = be_u32(v) {
                        g.distance_m = Some(m);
                    }
                }
                RG_CURRENT_MANEUVERS => {
                    // A list of indices; the first is the next manoeuvre.
                    if let Some(first) = v.get(..2).and_then(be_u16) {
                        route.current = Some(first);
                    }
                }
                _ => {}
            }
        }
        if !g.active {
            g.clear();
            route.clear();
            return true;
        }
        route.apply(g);
        return true;
    }
    let Some(params) = params_of(csm, MSG_ROUTE_GUIDANCE_MANEUVER) else {
        return false;
    };
    // The index comes first in the phone's messages, but nothing says it
    // must: the fields are read once to find it.
    let Some(index) = params
        .into_iter()
        .find(|(k, _)| *k == MAN_INDEX)
        .and_then(|(_, v)| be_u16(v))
    else {
        return true;
    };
    let slot = route.slot_mut(index);
    for (k, v) in params {
        match (k, v) {
            (MAN_TYPE, [t]) => slot.maneuver = maneuver(*t),
            (MAN_AFTER_ROAD, _) => set_text(&mut slot.road, text(v)),
            _ => {}
        }
    }
    if route.current == Some(index) && g.active {
        route.apply(g);
    }
    true
}

/// iAP2's RouteGuidanceManeuverType, 0 to 53, as LIVI's table names them.
pub fn maneuver(t: u8) -> Maneuver {
    match t {
        0 | 3 | 5 => Maneuver::Straight,
        1 | 20 => Maneuver::Left,
        2 | 21 => Maneuver::Right,
        4 | 18 | 19 | 26 => Maneuver::UTurn,
        6 | 7 => Maneuver::Roundabout(0),
        8 => Maneuver::Exit,
        9 | 51 => Maneuver::Merge,
        10 | 12 | 24 | 25 | 27 => Maneuver::Arrive,
        11 => Maneuver::Depart,
        13 | 52 => Maneuver::KeepLeft,
        14 | 53 => Maneuver::KeepRight,
        15..=17 => Maneuver::Ferry,
        22 => Maneuver::ExitLeft,
        23 => Maneuver::ExitRight,
        28..=46 => Maneuver::Roundabout(t - 27),
        47 => Maneuver::SharpLeft,
        48 => Maneuver::SharpRight,
        49 => Maneuver::SlightLeft,
        50 => Maneuver::SlightRight,
        _ => Maneuver::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iap2::{pack_params, parse_csm, parse_params};

    fn s(text: &str) -> Vec<u8> {
        let mut b = text.as_bytes().to_vec();
        b.push(0);
        b
    }

    #[test]
    fn the_subscriptions_ask_for_what_the_screens_show() {
        let csm = start_now_playing(false);
        let (id, params) = parse_csm(&csm).unwrap();
        assert_eq!(id, 0x5000);
        let item: Vec<u16> = parse_params(params[0].1)
            .unwrap()
            .iter()
            .map(|(k, v)| {
                assert!(v.is_empty(), "flags only");
                *k
            })
            .collect();
        assert_eq!((params[0].0, item), (0, vec![1, 4, 6, 12]));
        let play: Vec<u16> = parse_params(params[1].1)
            .unwrap()
            .iter()
            .map(|(k, _)| *k)
            .collect();
        assert_eq!((params[1].0, play), (1, vec![0, 1, 7]));
        assert_eq!(start_route_guidance(), [0x40, 0x40, 0, 6, 0x52, 0x00]);
        // With the art: its transfer id too.
        let with_art = start_now_playing(true);
        let (_, params) = parse_csm(&with_art).unwrap();
        let item: Vec<u16> = parse_params(params[0].1)
            .unwrap()
            .iter()
            .map(|(k, _)| *k)
            .collect();
        assert_eq!(item, vec![1, 4, 6, 12, 26]);
    }

    #[test]
    fn an_update_names_the_art_and_says_when_the_item_changed() {
        let update = |fields: &[(u16, &[u8])]| {
            pack_csm(MSG_NOW_PLAYING_UPDATE, &[(0, &pack_params(fields))])
        };
        let mut np = NowPlaying::default();
        let song = s("Song");
        let art = now_playing_update(
            &update(&[(ITEM_TITLE, &song), (ITEM_ARTWORK, &[7])]),
            &mut np,
            0,
        );
        assert_eq!(
            art,
            Some(ItemArt {
                new_item: true,
                transfer: Some(7)
            })
        );
        // The same title again is the same item.
        let art = now_playing_update(&update(&[(ITEM_TITLE, &song)]), &mut np, 0);
        assert_eq!(art, Some(ItemArt::default()));
        let art = now_playing_update(&update(&[(ITEM_ARTWORK, &[8])]), &mut np, 0);
        assert_eq!(
            art,
            Some(ItemArt {
                new_item: false,
                transfer: Some(8)
            })
        );
        // An id of the wrong size is no id.
        let art = now_playing_update(
            &update(&[(ITEM_TITLE, &s("Next")), (ITEM_ARTWORK, &[1, 2])]),
            &mut np,
            0,
        );
        assert_eq!(
            art,
            Some(ItemArt {
                new_item: true,
                transfer: None
            })
        );
    }

    #[test]
    fn a_now_playing_update_fills_the_item_and_runs_the_position() {
        let item = pack_params(&[
            (ITEM_TITLE, &s("Song")),
            (ITEM_DURATION_MS, &200_000u32.to_be_bytes()),
            (ITEM_ALBUM, &s("Album")),
            (ITEM_ARTIST, &s("Artist")),
            (99, &[1, 2, 3]),
        ]);
        let play = pack_params(&[
            (PLAY_STATUS, &[1]),
            (PLAY_ELAPSED_MS, &30_000u32.to_be_bytes()),
            (PLAY_APP_NAME, &s("Music")),
        ]);
        let csm = pack_csm(MSG_NOW_PLAYING_UPDATE, &[(0, &item), (1, &play)]);
        let mut np = NowPlaying::default();
        assert!(now_playing_update(&csm, &mut np, 1000).is_some());
        assert_eq!(
            (np.title.as_str(), np.artist.as_str(), np.album.as_str()),
            ("Song", "Artist", "Album")
        );
        assert_eq!((np.duration_ms, np.app.as_str()), (200_000, "Music"));
        assert_eq!(np.playback, Playback::Playing);
        assert_eq!(np.position(3000), 32_000);

        // Paused alone: the position stops where it got to.
        let paused = pack_csm(
            MSG_NOW_PLAYING_UPDATE,
            &[(1, &pack_params(&[(PLAY_STATUS, &[2])]))],
        );
        assert!(now_playing_update(&paused, &mut np, 11_000).is_some());
        assert_eq!(np.position(50_000), 40_000);

        // A new title is a new item: the old album does not stay.
        let next = pack_csm(
            MSG_NOW_PLAYING_UPDATE,
            &[(0, &pack_params(&[(ITEM_TITLE, &s("Other"))]))],
        );
        assert!(now_playing_update(&next, &mut np, 12_000).is_some());
        assert_eq!((np.title.as_str(), np.album.as_str()), ("Other", ""));
        assert_eq!(np.duration_ms, 0);
        // A duration alone keeps the title.
        let dur = pack_csm(
            MSG_NOW_PLAYING_UPDATE,
            &[(0, &pack_params(&[(ITEM_DURATION_MS, &5u32.to_be_bytes())]))],
        );
        assert!(now_playing_update(&dur, &mut np, 12_000).is_some());
        assert_eq!((np.title.as_str(), np.duration_ms), ("Other", 5));
    }

    #[test]
    fn the_now_playing_parser_rejects_before_it_indexes() {
        let mut np = NowPlaying::default();
        assert!(now_playing_update(&[], &mut np, 0).is_none());
        assert!(now_playing_update(&[0x40, 0x40, 0, 6, 0x50], &mut np, 0).is_none());
        assert!(now_playing_update(&start_now_playing(false), &mut np, 0).is_none());
        // The header claims more than there is.
        assert!(now_playing_update(&[0x40, 0x40, 0, 99, 0x50, 0x01], &mut np, 0).is_none());
        // Wrong sizes are skipped; a length past the end stops the walk.
        let item = pack_params(&[(ITEM_DURATION_MS, &[1, 2]), (ITEM_TITLE, &[0xff, 0xfe, 0])]);
        let mut csm = pack_csm(MSG_NOW_PLAYING_UPDATE, &[(0, &item)]);
        assert!(now_playing_update(&csm, &mut np, 0).is_some());
        assert_eq!((np.duration_ms, np.title.as_str()), (0, ""));
        csm.extend_from_slice(&[0x00, 0x40, 0x00, 0x01]);
        let len = csm.len() as u16;
        csm[2..4].copy_from_slice(&len.to_be_bytes());
        assert!(now_playing_update(&csm, &mut np, 0).is_some());
        // Every truncation of a good message is read without a panic.
        let good = pack_csm(
            MSG_NOW_PLAYING_UPDATE,
            &[(
                0,
                &pack_params(&[(ITEM_TITLE, &s("Song")), (ITEM_ARTIST, &s("A"))]),
            )],
        );
        for n in 0..good.len() {
            let mut cut = good[..n].to_vec();
            if n >= 4 {
                cut[2..4].copy_from_slice(&(n as u16).to_be_bytes());
            }
            now_playing_update(&cut, &mut np, 0);
        }
    }

    fn update(params: &[(u16, &[u8])]) -> Vec<u8> {
        pack_csm(MSG_ROUTE_GUIDANCE_UPDATE, params)
    }

    fn maneuver_update(index: u16, kind: u8, road: &str) -> Vec<u8> {
        pack_csm(
            MSG_ROUTE_GUIDANCE_MANEUVER,
            &[
                (0, &0u16.to_be_bytes()),
                (MAN_INDEX, &index.to_be_bytes()),
                (MAN_TYPE, &[kind]),
                (MAN_AFTER_ROAD, &s(road)),
            ],
        )
    }

    #[test]
    fn route_guidance_names_the_current_manoeuvre_by_index() {
        let mut route = Route::default();
        let mut g = Guidance::default();
        // The manoeuvres come first, then the update that names one.
        assert!(route_guidance(
            &maneuver_update(4, 2, "Main St"),
            &mut route,
            &mut g
        ));
        assert!(route_guidance(
            &maneuver_update(5, 29, "Ring Rd"),
            &mut route,
            &mut g
        ));
        assert!(!g.active);
        let list = [0u8, 4, 0, 5];
        let csm = update(&[
            (0, &0u16.to_be_bytes()),
            (RG_STATE, &[1]),
            (RG_CURRENT_ROAD, &s("High St")),
            (RG_ETA, &1_790_000_000u64.to_be_bytes()),
            (RG_TIME_REMAINING, &600u64.to_be_bytes()),
            (RG_DISTANCE_TO_MANEUVER, &350u32.to_be_bytes()),
            (RG_CURRENT_MANEUVERS, &list),
        ]);
        assert!(route_guidance(&csm, &mut route, &mut g));
        assert!(g.active);
        assert_eq!(g.maneuver, Maneuver::Right);
        assert_eq!(g.road, "Main St");
        assert_eq!(g.distance_m, Some(350));
        assert_eq!((g.eta_unix, g.remaining_s), (1_790_000_000, Some(600)));

        // The next one comes up; then it is described again, with no road:
        // the road driven on stands in.
        let next = update(&[(RG_CURRENT_MANEUVERS, &list[2..])]);
        assert!(route_guidance(&next, &mut route, &mut g));
        assert_eq!(
            (g.maneuver, g.road.as_str()),
            (Maneuver::Roundabout(2), "Ring Rd")
        );
        assert!(route_guidance(
            &maneuver_update(5, 47, ""),
            &mut route,
            &mut g
        ));
        assert_eq!(
            (g.maneuver, g.road.as_str()),
            (Maneuver::SharpLeft, "High St")
        );

        // Arrived: guidance ends and nothing of the route stays.
        assert!(route_guidance(
            &update(&[(RG_STATE, &[2])]),
            &mut route,
            &mut g
        ));
        assert_eq!(g, Guidance::default());
        assert!(!route_guidance(&start_route_guidance(), &mut route, &mut g));
    }

    #[test]
    fn the_route_keeps_a_few_manoeuvres_and_survives_bad_input() {
        let mut route = Route::default();
        let mut g = Guidance::default();
        for i in 0..20u16 {
            route_guidance(&maneuver_update(i, 1, "x"), &mut route, &mut g);
        }
        assert_eq!(
            route.slots.iter().filter(|s| s.index.is_some()).count(),
            ROUTE_SLOTS
        );
        // No index: nothing to file it under.
        let no_index = pack_csm(MSG_ROUTE_GUIDANCE_MANEUVER, &[(MAN_TYPE, &[1])]);
        assert!(route_guidance(&no_index, &mut route, &mut g));
        // Short values are skipped, an odd list is not read past its end.
        let bad = update(&[
            (RG_STATE, &[1, 1]),
            (RG_DISTANCE_TO_MANEUVER, &[1]),
            (RG_CURRENT_MANEUVERS, &[7]),
            (RG_ETA, &[0xff; 8]),
        ]);
        assert!(route_guidance(&bad, &mut route, &mut g));
        assert_eq!(g.distance_m, None);
        let good = maneuver_update(3, 1, "Road");
        for n in 0..good.len() {
            let mut cut = good[..n].to_vec();
            if n >= 4 {
                cut[2..4].copy_from_slice(&(n as u16).to_be_bytes());
            }
            route_guidance(&cut, &mut route, &mut g);
        }
    }

    #[test]
    fn the_manoeuvre_table() {
        assert_eq!(maneuver(1), Maneuver::Left);
        assert_eq!(maneuver(2), Maneuver::Right);
        assert_eq!(maneuver(12), Maneuver::Arrive);
        assert_eq!(maneuver(28), Maneuver::Roundabout(1));
        assert_eq!(maneuver(46), Maneuver::Roundabout(19));
        assert_eq!(maneuver(50), Maneuver::SlightRight);
        assert_eq!(maneuver(53), Maneuver::KeepRight);
        assert_eq!(maneuver(54), Maneuver::None);
        assert_eq!(maneuver(255), Maneuver::None);
    }
}
