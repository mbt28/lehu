// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
// Copyright (C) 2025-2026 Lasse Heitgres (LIVI, https://github.com/f-io/LIVI)
//! Now playing and turn-by-turn from the phone: the media playback status
//! channel and the navigation status channel, decoded into the session's
//! side channel. The phone only sends on these channels; the head unit
//! declared them (text only, no turn images) and answers their opening.
//! The album art comes in the track's metadata (field 4, PNG or JPEG) and
//! goes to the side channel as it came.
//! The ids, field numbers and enum values follow LIVI's protocol
//! definitions (see NOTICE).

use lehu_session::{Artwork, Guidance, Maneuver, NowPlaying, Playback, set_text};

use crate::proto::{self, decode};

// --- Media playback status channel ---
pub const MSG_PLAYBACK_STATUS: u16 = 0x8001;
pub const MSG_PLAYBACK_METADATA: u16 = 0x8003;

// --- Navigation status channel ---
pub const MSG_NAV_START: u16 = 0x8001;
pub const MSG_NAV_STOP: u16 = 0x8002;
pub const MSG_NAV_STATUS: u16 = 0x8003;
pub const MSG_NAV_NEXT_TURN: u16 = 0x8004;
pub const MSG_NAV_NEXT_TURN_DISTANCE: u16 = 0x8005;
pub const MSG_NAV_STATE: u16 = 0x8006;
pub const MSG_NAV_CURRENT_POSITION: u16 = 0x8007;

/// NavigationClusterType: the head unit draws its own card from text.
pub const NAV_CLUSTER_NO_IMAGE: i32 = 2;
/// The phone sends guidance at most this often.
pub const NAV_MIN_INTERVAL_MS: i32 = 1000;

/// A message on the media playback status channel into `np`; false when
/// it is not one or does not decode.
pub fn playback(id: u16, body: &[u8], np: &mut NowPlaying, now: u64) -> bool {
    match id {
        MSG_PLAYBACK_STATUS => {
            let Some(s) = decode::<proto::MediaPlaybackStatus>(body) else {
                return false;
            };
            if let Some(secs) = s.playback_position_seconds {
                np.set_position(secs.saturating_mul(1000), now);
            }
            if let Some(app) = &s.media_app_name {
                set_text(&mut np.app, app);
            }
            if let Some(state) = s.state {
                np.set_playback(
                    match state {
                        2 => Playback::Playing,
                        3 => Playback::Paused,
                        _ => Playback::Stopped,
                    },
                    now,
                );
            }
            true
        }
        MSG_PLAYBACK_METADATA => {
            let Some(m) = decode::<proto::MediaPlaybackMetadata>(body) else {
                return false;
            };
            // Each one describes the whole track.
            np.new_item();
            set_text(&mut np.title, m.title.as_deref().unwrap_or(""));
            set_text(&mut np.artist, m.subtitle.as_deref().unwrap_or(""));
            set_text(&mut np.album, m.description.as_deref().unwrap_or(""));
            np.duration_ms = m.duration_seconds.unwrap_or(0).saturating_mul(1000);
            true
        }
        _ => false,
    }
}

/// The album art of a metadata message into `art`: each one describes the
/// whole track, so one without art means none. False when the art did
/// not change, the message is not one or does not walk.
pub fn artwork(id: u16, body: &[u8], art: &mut Artwork) -> bool {
    if id != MSG_PLAYBACK_METADATA {
        return false;
    }
    match proto::bytes_field(body, 4) {
        Some(Some(data)) => art.set(data),
        Some(None) => art.clear(),
        None => false,
    }
}

/// A message on the navigation status channel into `g`; false when it is
/// not one or does not decode.
pub fn navigation(id: u16, body: &[u8], g: &mut Guidance) -> bool {
    match id {
        MSG_NAV_START => g.active = true,
        MSG_NAV_STOP => g.clear(),
        MSG_NAV_STATUS => {
            let Some(s) = decode::<proto::NavigationStatus>(body) else {
                return false;
            };
            // Active or rerouting; unavailable and inactive end it.
            if matches!(s.status, 1 | 3) {
                g.active = true;
            } else {
                g.clear();
            }
        }
        MSG_NAV_NEXT_TURN => {
            let Some(t) = decode::<proto::NavigationNextTurn>(body) else {
                return false;
            };
            g.active = true;
            g.maneuver = next_turn(
                t.turn_type.unwrap_or(0),
                t.turn_side.unwrap_or(3),
                t.roundabout_exit_number.unwrap_or(0),
            );
            set_text(&mut g.road, &t.road_name);
        }
        MSG_NAV_NEXT_TURN_DISTANCE => {
            let Some(d) = decode::<proto::NavigationNextTurnDistance>(body) else {
                return false;
            };
            g.active = true;
            g.distance_m = u32::try_from(d.distance_m).ok();
        }
        MSG_NAV_STATE => {
            let Some(s) = decode::<proto::NavigationState>(body) else {
                return false;
            };
            let Some(step) = s.steps.first() else {
                return true;
            };
            g.active = true;
            if let Some(m) = &step.maneuver {
                g.maneuver = maneuver_type(
                    m.maneuver_type.unwrap_or(0),
                    m.roundabout_exit_number.unwrap_or(0),
                );
            }
            if let Some(name) = step.road.as_ref().and_then(|r| r.name.as_deref()) {
                set_text(&mut g.road, name);
            }
        }
        MSG_NAV_CURRENT_POSITION => {
            let Some(p) = decode::<proto::NavigationCurrentPosition>(body) else {
                return false;
            };
            g.active = true;
            if let Some(m) = p
                .step_distance
                .as_ref()
                .and_then(|s| s.distance.as_ref())
                .and_then(|d| d.distance_m)
            {
                g.distance_m = u32::try_from(m).ok();
            }
            if let Some(d) = p.destination_distances.first() {
                set_text(
                    &mut g.eta_text,
                    d.estimated_time_of_arrival.as_deref().unwrap_or(""),
                );
                g.remaining_s = d
                    .time_to_arrival_seconds
                    .and_then(|s| u32::try_from(s).ok());
            }
            // The road driven on, only while no step named the next one.
            if g.road.is_empty()
                && let Some(name) = p.current_road.as_ref().and_then(|r| r.name.as_deref())
            {
                set_text(&mut g.road, name);
            }
        }
        _ => return false,
    }
    true
}

/// A roundabout exit as the card counts it: 0 when not said or absurd.
fn exit(n: i32) -> u8 {
    u8::try_from(n).ok().filter(|&n| n <= 20).unwrap_or(0)
}

/// NavigationNextTurnType with its side (1 left, 2 right, 3 not said).
fn next_turn(kind: i32, side: i32, roundabout: i32) -> Maneuver {
    let lr = |left: Maneuver, right: Maneuver, neither: Maneuver| match side {
        1 => left,
        2 => right,
        _ => neither,
    };
    match kind {
        1 => Maneuver::Depart,
        2 | 14 => Maneuver::Straight,
        3 => lr(Maneuver::SlightLeft, Maneuver::SlightRight, Maneuver::None),
        4 => lr(Maneuver::Left, Maneuver::Right, Maneuver::None),
        5 => lr(Maneuver::SharpLeft, Maneuver::SharpRight, Maneuver::None),
        6 => Maneuver::UTurn,
        7 => lr(Maneuver::KeepLeft, Maneuver::KeepRight, Maneuver::Merge),
        8 => lr(Maneuver::ExitLeft, Maneuver::ExitRight, Maneuver::Exit),
        9 => lr(Maneuver::KeepLeft, Maneuver::KeepRight, Maneuver::None),
        10 => Maneuver::Merge,
        11..=13 => Maneuver::Roundabout(exit(roundabout)),
        16 | 17 => Maneuver::Ferry,
        19 => Maneuver::Arrive,
        _ => Maneuver::None,
    }
}

/// NavigationManeuverType, the newer phones' kinds: the odd ones of a
/// left/right pair are the left.
fn maneuver_type(kind: i32, roundabout: i32) -> Maneuver {
    let left = kind % 2 == 1;
    let pick = |l: Maneuver, r: Maneuver| if left { l } else { r };
    match kind {
        1 => Maneuver::Depart,
        2 | 36 => Maneuver::Straight,
        3 => Maneuver::KeepLeft,
        4 => Maneuver::KeepRight,
        5 | 6 => pick(Maneuver::SlightLeft, Maneuver::SlightRight),
        7 | 8 => pick(Maneuver::Left, Maneuver::Right),
        9 | 10 => pick(Maneuver::SharpLeft, Maneuver::SharpRight),
        11 | 12 | 19 | 20 => Maneuver::UTurn,
        13..=18 => pick(Maneuver::KeepLeft, Maneuver::KeepRight),
        21..=24 => pick(Maneuver::ExitLeft, Maneuver::ExitRight),
        25 | 26 => pick(Maneuver::KeepLeft, Maneuver::KeepRight),
        27..=29 => Maneuver::Merge,
        32..=35 | 43..=46 => Maneuver::Roundabout(exit(roundabout)),
        37 | 38 | 47..=50 => Maneuver::Ferry,
        39..=42 => Maneuver::Arrive,
        _ => Maneuver::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::encode_into;

    fn bytes(m: &impl prost::Message) -> Vec<u8> {
        let mut buf = [0u8; 512];
        let n = encode_into(m, &mut buf).expect("fits");
        buf[..n].to_vec()
    }

    #[test]
    fn the_playback_messages_round_trip_into_now_playing() {
        let meta = proto::MediaPlaybackMetadata {
            title: Some("Song".into()),
            subtitle: Some("Artist".into()),
            description: Some("Album".into()),
            duration_seconds: Some(215),
        };
        let body = bytes(&meta);
        assert_eq!(decode::<proto::MediaPlaybackMetadata>(&body), Some(meta));
        // As the phone sends it, with the album art in field 4 between.
        let mut with_art = body.clone();
        with_art.extend_from_slice(&[0x22, 3, 0x89, b'P', b'N']);
        let mut np = NowPlaying::default();
        assert!(playback(MSG_PLAYBACK_METADATA, &with_art, &mut np, 0));
        assert_eq!(
            (np.title.as_str(), np.artist.as_str(), np.album.as_str()),
            ("Song", "Artist", "Album")
        );
        assert_eq!(np.duration_ms, 215_000);
        // The art, as it came; a track without art has none.
        let mut art = Artwork::default();
        assert!(artwork(MSG_PLAYBACK_METADATA, &with_art, &mut art));
        assert_eq!(art.data, [0x89, b'P', b'N']);
        assert!(
            !artwork(MSG_PLAYBACK_METADATA, &with_art, &mut art),
            "the same art"
        );
        assert!(!artwork(MSG_PLAYBACK_STATUS, &body, &mut art));
        assert!(
            !artwork(MSG_PLAYBACK_METADATA, &[0x22, 9, 1], &mut art),
            "cut short"
        );
        assert!(artwork(MSG_PLAYBACK_METADATA, &body, &mut art));
        assert!(art.data.is_empty());

        let status = proto::MediaPlaybackStatus {
            state: Some(2),
            media_app_name: Some("Spotify".into()),
            playback_position_seconds: Some(14),
        };
        let body = bytes(&status);
        assert_eq!(decode::<proto::MediaPlaybackStatus>(&body), Some(status));
        assert!(playback(MSG_PLAYBACK_STATUS, &body, &mut np, 1000));
        assert_eq!(np.playback, Playback::Playing);
        assert_eq!((np.position(3000), np.app.as_str()), (16_000, "Spotify"));
        let paused = bytes(&proto::MediaPlaybackStatus {
            state: Some(3),
            ..Default::default()
        });
        assert!(playback(MSG_PLAYBACK_STATUS, &paused, &mut np, 2000));
        assert_eq!((np.playback, np.position(9000)), (Playback::Paused, 15_000));
        // A new track: what it does not say is gone.
        let next = bytes(&proto::MediaPlaybackMetadata {
            title: Some("Next".into()),
            ..Default::default()
        });
        assert!(playback(MSG_PLAYBACK_METADATA, &next, &mut np, 0));
        assert_eq!(
            (np.title.as_str(), np.album.as_str(), np.duration_ms),
            ("Next", "", 0)
        );
        // Not a message of this channel, or one that does not decode.
        assert!(!playback(0x8002, &[], &mut np, 0));
        assert!(!playback(MSG_PLAYBACK_STATUS, &[0x08], &mut np, 0));
        assert!(!playback(
            MSG_PLAYBACK_METADATA,
            &[0x0a, 0x09, b'x'],
            &mut np,
            0
        ));
    }

    #[test]
    fn the_classic_navigation_messages_round_trip_into_guidance() {
        let mut g = Guidance::default();
        assert!(navigation(MSG_NAV_START, &[], &mut g));
        assert!(g.active);
        let turn = proto::NavigationNextTurn {
            road_name: "Main St".into(),
            turn_side: Some(1),
            turn_type: Some(4),
            roundabout_exit_number: None,
        };
        let body = bytes(&turn);
        assert_eq!(decode::<proto::NavigationNextTurn>(&body), Some(turn));
        // The image the phone was told not to send would be skipped.
        let mut with_image = body.clone();
        with_image.extend_from_slice(&[0x22, 2, 1, 2]);
        assert!(navigation(MSG_NAV_NEXT_TURN, &with_image, &mut g));
        assert_eq!((g.maneuver, g.road.as_str()), (Maneuver::Left, "Main St"));
        let distance = proto::NavigationNextTurnDistance {
            distance_m: 350,
            time_to_turn_seconds: 20,
        };
        let body = bytes(&distance);
        assert_eq!(
            decode::<proto::NavigationNextTurnDistance>(&body),
            Some(distance)
        );
        assert!(navigation(MSG_NAV_NEXT_TURN_DISTANCE, &body, &mut g));
        assert_eq!(g.distance_m, Some(350));
        let negative = bytes(&proto::NavigationNextTurnDistance {
            distance_m: -5,
            time_to_turn_seconds: 0,
        });
        assert!(navigation(MSG_NAV_NEXT_TURN_DISTANCE, &negative, &mut g));
        assert_eq!(g.distance_m, None);
        let roundabout = bytes(&proto::NavigationNextTurn {
            road_name: "Ring".into(),
            turn_side: Some(2),
            turn_type: Some(13),
            roundabout_exit_number: Some(3),
        });
        assert!(navigation(MSG_NAV_NEXT_TURN, &roundabout, &mut g));
        assert_eq!(g.maneuver, Maneuver::Roundabout(3));
        // Inactive ends it; rerouting keeps it.
        let status = |s| bytes(&proto::NavigationStatus { status: s });
        assert!(navigation(MSG_NAV_STATUS, &status(3), &mut g));
        assert!(g.active && g.road == "Ring");
        assert!(navigation(MSG_NAV_STATUS, &status(2), &mut g));
        assert_eq!(g, Guidance::default());
        assert!(navigation(MSG_NAV_START, &[], &mut g));
        assert!(navigation(MSG_NAV_STOP, &[], &mut g));
        assert_eq!(g, Guidance::default());
        assert!(!navigation(0x8009, &[], &mut g));
        assert!(!navigation(MSG_NAV_NEXT_TURN, &[0x0a, 0x7f], &mut g));
    }

    #[test]
    fn the_newer_navigation_messages_round_trip_into_guidance() {
        let state = proto::NavigationState {
            steps: vec![
                proto::NavigationStep {
                    maneuver: Some(proto::NavigationManeuver {
                        maneuver_type: Some(24),
                        roundabout_exit_number: None,
                    }),
                    road: Some(proto::NavigationRoad {
                        name: Some("A1".into()),
                    }),
                },
                proto::NavigationStep {
                    maneuver: Some(proto::NavigationManeuver {
                        maneuver_type: Some(7),
                        roundabout_exit_number: None,
                    }),
                    road: None,
                },
            ],
        };
        let body = bytes(&state);
        assert_eq!(decode::<proto::NavigationState>(&body), Some(state));
        let mut g = Guidance::default();
        assert!(navigation(MSG_NAV_STATE, &body, &mut g));
        assert!(g.active);
        assert_eq!((g.maneuver, g.road.as_str()), (Maneuver::ExitRight, "A1"));
        let position = proto::NavigationCurrentPosition {
            step_distance: Some(proto::NavigationStepDistance {
                distance: Some(proto::NavigationDistance {
                    distance_m: Some(1200),
                }),
            }),
            destination_distances: vec![proto::NavigationDestinationDistance {
                estimated_time_of_arrival: Some("14:32".into()),
                time_to_arrival_seconds: Some(900),
            }],
            current_road: Some(proto::NavigationRoad {
                name: Some("High St".into()),
            }),
        };
        let body = bytes(&position);
        assert_eq!(
            decode::<proto::NavigationCurrentPosition>(&body),
            Some(position)
        );
        assert!(navigation(MSG_NAV_CURRENT_POSITION, &body, &mut g));
        assert_eq!(g.distance_m, Some(1200));
        assert_eq!((g.eta_text.as_str(), g.remaining_s), ("14:32", Some(900)));
        assert_eq!(g.road, "A1", "the step's road, not the one driven on");
        // No steps: nothing changes, and it is no error.
        assert!(navigation(MSG_NAV_STATE, &[], &mut g));
        assert_eq!(g.maneuver, Maneuver::ExitRight);
    }

    #[test]
    fn the_turn_tables() {
        assert_eq!(next_turn(3, 2, 0), Maneuver::SlightRight);
        assert_eq!(next_turn(4, 3, 0), Maneuver::None, "a turn with no side");
        assert_eq!(next_turn(8, 3, 0), Maneuver::Exit);
        assert_eq!(next_turn(11, 1, 99), Maneuver::Roundabout(0));
        assert_eq!(next_turn(19, 0, 0), Maneuver::Arrive);
        assert_eq!(next_turn(-1, 0, 0), Maneuver::None);
        assert_eq!(maneuver_type(5, 0), Maneuver::SlightLeft);
        assert_eq!(maneuver_type(10, 0), Maneuver::SharpRight);
        assert_eq!(maneuver_type(20, 0), Maneuver::UTurn);
        assert_eq!(maneuver_type(25, 0), Maneuver::KeepLeft);
        assert_eq!(maneuver_type(34, 2), Maneuver::Roundabout(2));
        assert_eq!(maneuver_type(41, 0), Maneuver::Arrive);
        assert_eq!(maneuver_type(-7, 0), Maneuver::None);
        assert_eq!(maneuver_type(51, 0), Maneuver::None);
    }
}
