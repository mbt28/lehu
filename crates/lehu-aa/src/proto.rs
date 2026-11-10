// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
// Copyright (C) 2025-2026 Lasse Heitgres (LIVI, https://github.com/f-io/LIVI)
//! The protobuf bodies the head unit builds and parses, written by hand
//! for the messages it uses rather than generated from the whole protocol:
//! prost's derive gives the encoding, the field numbers come from the
//! vendored definitions (LIVI's aap_protobuf and livi-aa-proto; see
//! NOTICE), and `ping_config`
//! follows LIVI's livi-aa/src/session.rs (Lasse Heitgres, GPL-3.0).
//! Builders encode into a caller's buffer and fail
//! rather than grow it, so the outgoing path allocates nothing; parsers
//! take a slice and reject what does not decode.
//!
//! proto2 `required` fields are plain, `optional` are `Option`, and enums
//! travel as `i32` with the values in `consts`.

use lehu_core::settings::VideoCodec;
use lehu_session::Codec;
use lehu_video::Caps;
use prost::Message;

use crate::consts::*;

/// A body did not fit the buffer it was to be encoded into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooBig;

// --- Control channel ---

#[derive(Clone, PartialEq, Message)]
pub struct PhoneInfo {
    /// The phone's lasting id, the key of the device registry.
    #[prost(string, optional, tag = "1")]
    pub instance_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ServiceDiscoveryRequest {
    /// The phone's name, for the status line. The icons and the rest of
    /// the request are skipped as unknown fields.
    #[prost(string, optional, tag = "5")]
    pub device_name: Option<String>,
    #[prost(message, optional, tag = "6")]
    pub phone_info: Option<PhoneInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Sensor {
    #[prost(int32, required, tag = "1")]
    pub sensor_type: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct SensorSourceService {
    #[prost(message, repeated, tag = "1")]
    pub sensors: Vec<Sensor>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AudioConfiguration {
    #[prost(uint32, required, tag = "1")]
    pub sampling_rate: u32,
    #[prost(uint32, required, tag = "2")]
    pub number_of_bits: u32,
    #[prost(uint32, required, tag = "3")]
    pub number_of_channels: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct VideoConfiguration {
    #[prost(int32, optional, tag = "1")]
    pub codec_resolution: Option<i32>,
    #[prost(int32, optional, tag = "2")]
    pub frame_rate: Option<i32>,
    #[prost(uint32, optional, tag = "3")]
    pub width_margin: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub height_margin: Option<u32>,
    #[prost(uint32, optional, tag = "5")]
    pub density: Option<u32>,
    #[prost(uint32, optional, tag = "8")]
    pub pixel_aspect_ratio_e4: Option<u32>,
    #[prost(int32, optional, tag = "10")]
    pub video_codec_type: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct MediaSinkService {
    #[prost(int32, optional, tag = "1")]
    pub available_type: Option<i32>,
    #[prost(int32, optional, tag = "2")]
    pub audio_type: Option<i32>,
    #[prost(message, repeated, tag = "3")]
    pub audio_configs: Vec<AudioConfiguration>,
    #[prost(message, repeated, tag = "4")]
    pub video_configs: Vec<VideoConfiguration>,
    #[prost(bool, optional, tag = "5")]
    pub available_while_in_call: Option<bool>,
    #[prost(uint32, optional, tag = "6")]
    pub display_id: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct MediaSourceService {
    #[prost(int32, optional, tag = "1")]
    pub available_type: Option<i32>,
    #[prost(message, optional, tag = "2")]
    pub audio_config: Option<AudioConfiguration>,
    #[prost(bool, optional, tag = "3")]
    pub available_while_in_call: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct TouchScreen {
    #[prost(int32, required, tag = "1")]
    pub width: i32,
    #[prost(int32, required, tag = "2")]
    pub height: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct InputSourceService {
    #[prost(int32, repeated, packed = "true", tag = "1")]
    pub keycodes_supported: Vec<i32>,
    #[prost(message, repeated, tag = "2")]
    pub touchscreen: Vec<TouchScreen>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Service {
    #[prost(int32, required, tag = "1")]
    pub id: i32,
    #[prost(message, optional, tag = "2")]
    pub sensor_source_service: Option<SensorSourceService>,
    #[prost(message, optional, tag = "3")]
    pub media_sink_service: Option<MediaSinkService>,
    #[prost(message, optional, tag = "4")]
    pub input_source_service: Option<InputSourceService>,
    #[prost(message, optional, tag = "5")]
    pub media_source_service: Option<MediaSourceService>,
    #[prost(message, optional, tag = "8")]
    pub navigation_status_service: Option<NavigationStatusService>,
    #[prost(message, optional, tag = "9")]
    pub media_playback_service: Option<MediaPlaybackStatusService>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PingConfiguration {
    #[prost(uint32, optional, tag = "1")]
    pub timeout_ms: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub interval_ms: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub high_latency_threshold_ms: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub tracked_ping_count: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConnectionConfiguration {
    #[prost(message, optional, tag = "1")]
    pub ping_configuration: Option<PingConfiguration>,
}

/// The bytes after the six of the version response: the phone's own
/// connection wishes, among them how often it wants to be pinged.
#[derive(Clone, PartialEq, Message)]
pub struct VersionResponseOptions {
    #[prost(message, optional, tag = "1")]
    pub connection_configuration: Option<ConnectionConfiguration>,
}

#[derive(Clone, PartialEq, Message)]
pub struct HeadUnitInfo {
    #[prost(string, optional, tag = "1")]
    pub make: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub model: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub year: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub vehicle_id: Option<String>,
    #[prost(string, optional, tag = "5")]
    pub head_unit_make: Option<String>,
    #[prost(string, optional, tag = "6")]
    pub head_unit_model: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub head_unit_software_build: Option<String>,
    #[prost(string, optional, tag = "8")]
    pub head_unit_software_version: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ServiceDiscoveryResponse {
    #[prost(message, repeated, tag = "1")]
    pub channels: Vec<Service>,
    #[prost(string, optional, tag = "2")]
    pub make: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub model: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub year: Option<String>,
    #[prost(string, optional, tag = "5")]
    pub vehicle_id: Option<String>,
    #[prost(int32, optional, tag = "6")]
    pub driver_position: Option<i32>,
    #[prost(bool, optional, tag = "11")]
    pub can_play_native_media_during_vr: Option<bool>,
    #[prost(string, optional, tag = "14")]
    pub display_name: Option<String>,
    #[prost(bool, optional, tag = "15")]
    pub probe_for_support: Option<bool>,
    #[prost(message, optional, tag = "16")]
    pub connection_configuration: Option<ConnectionConfiguration>,
    #[prost(message, optional, tag = "17")]
    pub headunit_info: Option<HeadUnitInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChannelOpenRequest {
    #[prost(sint32, required, tag = "1")]
    pub priority: i32,
    #[prost(int32, required, tag = "2")]
    pub service_id: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChannelOpenResponse {
    #[prost(int32, required, tag = "1")]
    pub status: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct AuthResponse {
    #[prost(int32, required, tag = "1")]
    pub status: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct PingRequest {
    #[prost(int64, required, tag = "1")]
    pub timestamp: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct PingResponse {
    #[prost(int64, required, tag = "1")]
    pub timestamp: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct AudioFocusRequest {
    #[prost(int32, required, tag = "1")]
    pub audio_focus_type: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct AudioFocusNotification {
    #[prost(int32, required, tag = "1")]
    pub focus_state: i32,
    #[prost(bool, optional, tag = "2")]
    pub unsolicited: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct NavFocusRequestNotification {
    #[prost(int32, optional, tag = "1")]
    pub focus_type: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct NavFocusNotification {
    #[prost(int32, required, tag = "1")]
    pub focus_type: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ByeByeRequest {
    #[prost(int32, required, tag = "1")]
    pub reason: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ByeByeResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct VoiceSessionNotification {
    #[prost(int32, optional, tag = "1")]
    pub status: Option<i32>,
}

// --- Media channels ---

#[derive(Clone, PartialEq, Message)]
pub struct MediaSetup {
    #[prost(int32, required, tag = "1")]
    pub codec_type: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct MediaConfig {
    #[prost(int32, required, tag = "1")]
    pub status: i32,
    #[prost(uint32, optional, tag = "2")]
    pub max_unacked: Option<u32>,
    #[prost(uint32, repeated, packed = "false", tag = "3")]
    pub configuration_indices: Vec<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct MediaStart {
    #[prost(int32, required, tag = "1")]
    pub session_id: i32,
    #[prost(uint32, required, tag = "2")]
    pub configuration_index: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct MediaAck {
    #[prost(int32, required, tag = "1")]
    pub session_id: i32,
    #[prost(uint32, optional, tag = "2")]
    pub ack: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct MicrophoneRequest {
    #[prost(bool, required, tag = "1")]
    pub open: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct MicrophoneResponse {
    #[prost(int32, required, tag = "1")]
    pub status: i32,
    #[prost(int32, optional, tag = "2")]
    pub session_id: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct VideoFocusRequestNotification {
    #[prost(int32, optional, tag = "2")]
    pub mode: Option<i32>,
    #[prost(int32, optional, tag = "3")]
    pub reason: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct VideoFocusNotification {
    #[prost(int32, optional, tag = "1")]
    pub focus: Option<i32>,
    #[prost(bool, optional, tag = "2")]
    pub unsolicited: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct KeyBindingResponse {
    #[prost(int32, required, tag = "1")]
    pub status: i32,
}

// --- Input channel ---

#[derive(Clone, PartialEq, Message)]
pub struct Pointer {
    #[prost(uint32, required, tag = "1")]
    pub x: u32,
    #[prost(uint32, required, tag = "2")]
    pub y: u32,
    #[prost(uint32, required, tag = "3")]
    pub pointer_id: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct TouchEvent {
    #[prost(message, repeated, tag = "1")]
    pub pointer_data: Vec<Pointer>,
    #[prost(uint32, optional, tag = "2")]
    pub action_index: Option<u32>,
    #[prost(int32, optional, tag = "3")]
    pub action: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Key {
    #[prost(uint32, required, tag = "1")]
    pub keycode: u32,
    #[prost(bool, required, tag = "2")]
    pub down: bool,
    #[prost(uint32, required, tag = "3")]
    pub metastate: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct KeyEvent {
    #[prost(message, repeated, tag = "1")]
    pub keys: Vec<Key>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Rel {
    #[prost(uint32, required, tag = "1")]
    pub keycode: u32,
    #[prost(int32, required, tag = "2")]
    pub delta: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct RelativeEvent {
    #[prost(message, repeated, tag = "1")]
    pub data: Vec<Rel>,
}

#[derive(Clone, PartialEq, Message)]
pub struct InputReport {
    #[prost(uint64, required, tag = "1")]
    pub timestamp: u64,
    #[prost(message, optional, tag = "3")]
    pub touch_event: Option<TouchEvent>,
    #[prost(message, optional, tag = "4")]
    pub key_event: Option<KeyEvent>,
    #[prost(message, optional, tag = "6")]
    pub relative_event: Option<RelativeEvent>,
}

// --- Sensor channel ---

#[derive(Clone, PartialEq, Message)]
pub struct SensorRequest {
    #[prost(int32, required, tag = "1")]
    pub sensor_type: i32,
    #[prost(int64, required, tag = "2")]
    pub min_update_period: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct SensorStartResponse {
    #[prost(int32, required, tag = "1")]
    pub status: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct NightModeData {
    #[prost(bool, optional, tag = "1")]
    pub night_mode: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DrivingStatusData {
    #[prost(int32, required, tag = "1")]
    pub status: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct SensorBatch {
    #[prost(message, repeated, tag = "10")]
    pub night_mode_data: Vec<NightModeData>,
    #[prost(message, repeated, tag = "13")]
    pub driving_status_data: Vec<DrivingStatusData>,
}

// --- Media playback status and navigation status services ---

/// The navigation status service: turn-by-turn as text, no images.
#[derive(Clone, PartialEq, Message)]
pub struct NavigationStatusService {
    #[prost(int32, required, tag = "1")]
    pub minimum_interval_ms: i32,
    #[prost(int32, required, tag = "2")]
    pub cluster_type: i32,
}

/// The media playback status service has no options.
#[derive(Clone, PartialEq, Message)]
pub struct MediaPlaybackStatusService {}

#[derive(Clone, PartialEq, Message)]
pub struct MediaPlaybackStatus {
    #[prost(int32, optional, tag = "1")]
    pub state: Option<i32>,
    #[prost(string, optional, tag = "2")]
    pub media_app_name: Option<String>,
    #[prost(uint32, optional, tag = "3")]
    pub playback_position_seconds: Option<u32>,
}

/// The track. Its album art (field 4) is read in place with
/// `bytes_field`, not copied by the decode.
#[derive(Clone, PartialEq, Message)]
pub struct MediaPlaybackMetadata {
    #[prost(string, optional, tag = "1")]
    pub title: Option<String>,
    /// The artist.
    #[prost(string, optional, tag = "2")]
    pub subtitle: Option<String>,
    /// The album.
    #[prost(string, optional, tag = "3")]
    pub description: Option<String>,
    #[prost(uint32, optional, tag = "6")]
    pub duration_seconds: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct NavigationStatus {
    #[prost(int32, required, tag = "1")]
    pub status: i32,
}

/// The next turn: its image (field 4) is never asked for.
#[derive(Clone, PartialEq, Message)]
pub struct NavigationNextTurn {
    #[prost(string, required, tag = "1")]
    pub road_name: String,
    #[prost(int32, optional, tag = "2")]
    pub turn_side: Option<i32>,
    #[prost(int32, optional, tag = "3")]
    pub turn_type: Option<i32>,
    #[prost(int32, optional, tag = "5")]
    pub roundabout_exit_number: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct NavigationNextTurnDistance {
    #[prost(int32, required, tag = "1")]
    pub distance_m: i32,
    #[prost(int32, required, tag = "2")]
    pub time_to_turn_seconds: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct NavigationManeuver {
    #[prost(int32, optional, tag = "1")]
    pub maneuver_type: Option<i32>,
    #[prost(int32, optional, tag = "2")]
    pub roundabout_exit_number: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct NavigationRoad {
    #[prost(string, optional, tag = "1")]
    pub name: Option<String>,
}

/// One step of the route; its lanes and cue are left unread.
#[derive(Clone, PartialEq, Message)]
pub struct NavigationStep {
    #[prost(message, optional, tag = "1")]
    pub maneuver: Option<NavigationManeuver>,
    #[prost(message, optional, tag = "2")]
    pub road: Option<NavigationRoad>,
}

/// The newer phones' route: the steps ahead, the first the next one.
#[derive(Clone, PartialEq, Message)]
pub struct NavigationState {
    #[prost(message, repeated, tag = "1")]
    pub steps: Vec<NavigationStep>,
}

#[derive(Clone, PartialEq, Message)]
pub struct NavigationDistance {
    #[prost(int32, optional, tag = "1")]
    pub distance_m: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct NavigationStepDistance {
    #[prost(message, optional, tag = "1")]
    pub distance: Option<NavigationDistance>,
}

#[derive(Clone, PartialEq, Message)]
pub struct NavigationDestinationDistance {
    #[prost(string, optional, tag = "2")]
    pub estimated_time_of_arrival: Option<String>,
    #[prost(int64, optional, tag = "3")]
    pub time_to_arrival_seconds: Option<i64>,
}

/// The newer phones' position on the route: to the next step, to the
/// destinations, and the road driven on.
#[derive(Clone, PartialEq, Message)]
pub struct NavigationCurrentPosition {
    #[prost(message, optional, tag = "1")]
    pub step_distance: Option<NavigationStepDistance>,
    #[prost(message, repeated, tag = "2")]
    pub destination_distances: Vec<NavigationDestinationDistance>,
    #[prost(message, optional, tag = "3")]
    pub current_road: Option<NavigationRoad>,
}

/// Encode `m` into the front of `buf`; how many bytes it took.
pub fn encode_into(m: &impl Message, buf: &mut [u8]) -> Result<usize, TooBig> {
    let total = buf.len();
    let mut slot = &mut buf[..];
    m.encode(&mut slot).map_err(|_| TooBig)?;
    Ok(total - slot.len())
}

/// Decode a body; `None` when it is not a valid message of that shape.
pub fn decode<M: Message + Default>(body: &[u8]) -> Option<M> {
    M::decode(body).ok()
}

/// The last value of length-delimited field `tag` in a body, borrowed
/// rather than copied (the album art is tens of kilobytes): `Some(None)`
/// when the body has none, `None` when it does not walk as protobuf.
pub fn bytes_field(body: &[u8], tag: u64) -> Option<Option<&[u8]>> {
    let mut b = body;
    let mut found = None;
    while !b.is_empty() {
        let key = varint(&mut b)?;
        match key & 7 {
            0 => {
                varint(&mut b)?;
            }
            1 => b = b.get(8..)?,
            2 => {
                let n = usize::try_from(varint(&mut b)?).ok()?;
                let value = b.get(..n)?;
                if key >> 3 == tag {
                    found = Some(value);
                }
                b = &b[n..];
            }
            5 => b = b.get(4..)?,
            // Groups: in none of the messages read here.
            _ => return None,
        }
    }
    Some(found)
}

/// A protobuf varint off the front of `b`: at most ten bytes.
fn varint(b: &mut &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for i in 0..10 {
        let (&byte, rest) = b.split_first()?;
        *b = rest;
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// What the head unit offers: the pixel size of the video and the touch
/// space, the codecs, the frame rate, the density and which side the
/// driver sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offer {
    /// 1 = 800x480, 2 = 1280x720, 3 = 1920x1080; the only sizes the
    /// protocol knows, as its video configuration carries an enum, not a
    /// width and height.
    pub resolution: u32,
    /// One video configuration each, in this order; the phone picks one.
    pub codecs: [Option<Codec>; 2],
    pub fps: u32,
    pub dpi: u32,
    pub left_hand_drive: bool,
    pub max_unacked: u32,
}

/// The pixel size of a resolution tier.
fn tier_size(tier: u32) -> (u32, u32) {
    match tier {
        3 => (1920, 1080),
        2 => (1280, 720),
        _ => (800, 480),
    }
}

/// The protocol's codec type for a codec, and back; VP9 and AV1 are
/// never offered, so a setup naming them names nothing we offered.
pub fn codec_type(codec: Codec) -> i32 {
    match codec {
        Codec::H264 => CODEC_VIDEO_H264_BP,
        Codec::Hevc => CODEC_VIDEO_H265,
    }
}

pub fn codec_of(codec_type: i32) -> Option<Codec> {
    match codec_type {
        CODEC_VIDEO_H264_BP => Some(Codec::H264),
        CODEC_VIDEO_H265 => Some(Codec::Hevc),
        _ => None,
    }
}

impl Offer {
    /// The pixel size behind the resolution setting, which is also the
    /// touch coordinate space.
    pub fn size(&self) -> (u32, u32) {
        tier_size(self.resolution)
    }

    fn resolution_type(&self) -> i32 {
        match self.resolution {
            3 => 3,
            2 => 2,
            _ => 1,
        }
    }

    /// The codecs offered, in order.
    pub fn codecs(&self) -> impl Iterator<Item = Codec> + '_ {
        self.codecs.iter().flatten().copied()
    }

    /// The configuration the phone's codec type picks: its place in the
    /// offer, the first for one we did not offer.
    pub fn config_index(&self, codec_type: i32) -> u32 {
        codec_of(codec_type)
            .and_then(|c| self.codecs().position(|o| o == c))
            .and_then(|i| u32::try_from(i).ok())
            .unwrap_or(0)
    }

    /// Fit the video to what the system decodes: the codecs from the
    /// probe and `display.codec`, and the resolution setting lowered to
    /// the largest tier every offered decoder takes. A codec offered with
    /// no decoder behind it (H.264 as the baseline) holds nothing down.
    pub fn fit(&mut self, caps: &Caps, wanted: VideoCodec) {
        let mut list = lehu_video::android_codecs(caps, wanted);
        // A decoder that cannot take the smallest tier is none here.
        list.retain(|&c| !caps.has(c) || caps.fits(c, 800, 480));
        if list.is_empty() {
            list.push(Codec::H264);
        }
        let asked = self.resolution_type() as u32;
        let mut tier = asked;
        while tier > 1
            && list.iter().any(|&c| {
                let (w, h) = tier_size(tier);
                caps.has(c) && !caps.fits(c, w, h)
            })
        {
            tier -= 1;
        }
        if tier != asked {
            let (aw, ah) = tier_size(asked);
            let (w, h) = tier_size(tier);
            log::warn!("android auto: {aw}x{ah} is beyond the decoder; offering {w}x{h}");
        }
        self.resolution = tier;
        self.codecs = [list.first().copied(), list.get(1).copied()];
    }
}

fn audio_sink(id: u8, stream: i32, rate: u32, channels: u32) -> Service {
    Service {
        id: i32::from(id),
        media_sink_service: Some(MediaSinkService {
            available_type: Some(CODEC_AUDIO_PCM),
            audio_type: Some(stream),
            audio_configs: vec![AudioConfiguration {
                sampling_rate: rate,
                number_of_bits: 16,
                number_of_channels: channels,
            }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The service discovery response: the sensors (driving status is
/// mandatory for projection, night mode drives the theme), the video sink
/// with a configuration per offered codec, the three PCM audio sinks (media 48 kHz stereo, guidance and
/// system 16 kHz mono), the microphone source (phones reject a head unit
/// without one), the touchscreen with the keys and the rotary controller,
/// the navigation status (turns as text, no images) and media playback
/// status services, the identification and the ping cadence (some phones
/// reject a response without a connection configuration).
pub fn service_discovery_response(offer: &Offer) -> ServiceDiscoveryResponse {
    let (width, height) = offer.size();
    let channels = vec![
        Service {
            id: i32::from(CH_SENSOR),
            sensor_source_service: Some(SensorSourceService {
                sensors: vec![
                    Sensor {
                        sensor_type: SENSOR_DRIVING_STATUS,
                    },
                    Sensor {
                        sensor_type: SENSOR_NIGHT_MODE,
                    },
                ],
            }),
            ..Default::default()
        },
        Service {
            id: i32::from(CH_VIDEO),
            media_sink_service: Some(MediaSinkService {
                available_type: Some(codec_type(offer.codecs().next().unwrap_or(Codec::H264))),
                available_while_in_call: Some(true),
                display_id: Some(0),
                video_configs: offer
                    .codecs()
                    .map(|c| VideoConfiguration {
                        codec_resolution: Some(offer.resolution_type()),
                        frame_rate: Some(if offer.fps == 60 { FPS_60 } else { FPS_30 }),
                        width_margin: Some(0),
                        height_margin: Some(0),
                        density: Some(offer.dpi),
                        pixel_aspect_ratio_e4: Some(10000),
                        // The phone maps each configuration to a codec
                        // through this; without it the configuration is
                        // ambiguous and the whole response is rejected.
                        video_codec_type: Some(codec_type(c)),
                    })
                    .collect(),
                ..Default::default()
            }),
            ..Default::default()
        },
        audio_sink(CH_MEDIA_AUDIO, AUDIO_STREAM_MEDIA, 48000, 2),
        audio_sink(CH_SPEECH_AUDIO, AUDIO_STREAM_GUIDANCE, 16000, 1),
        audio_sink(CH_SYSTEM_AUDIO, AUDIO_STREAM_SYSTEM, 16000, 1),
        Service {
            id: i32::from(CH_MIC),
            media_source_service: Some(MediaSourceService {
                available_type: Some(CODEC_AUDIO_PCM),
                audio_config: Some(AudioConfiguration {
                    sampling_rate: 16000,
                    number_of_bits: 16,
                    number_of_channels: 1,
                }),
                available_while_in_call: Some(true),
            }),
            ..Default::default()
        },
        Service {
            id: i32::from(CH_INPUT),
            input_source_service: Some(InputSourceService {
                keycodes_supported: KEYCODES.to_vec(),
                touchscreen: vec![TouchScreen {
                    width: width as i32,
                    height: height as i32,
                }],
            }),
            ..Default::default()
        },
        Service {
            id: i32::from(CH_NAVIGATION),
            navigation_status_service: Some(NavigationStatusService {
                minimum_interval_ms: crate::status::NAV_MIN_INTERVAL_MS,
                cluster_type: crate::status::NAV_CLUSTER_NO_IMAGE,
            }),
            ..Default::default()
        },
        Service {
            id: i32::from(CH_MEDIA_INFO),
            media_playback_service: Some(MediaPlaybackStatusService {}),
            ..Default::default()
        },
    ];
    let s = |t: &str| Some(t.to_string());
    ServiceDiscoveryResponse {
        channels,
        // The deprecated top-level strings are what older phone versions
        // show; headunit_info is the current home.
        make: s("LEHU"),
        model: s("LEHU"),
        year: s("2026"),
        vehicle_id: s("20260001"),
        driver_position: Some(if offer.left_hand_drive {
            DRIVER_POSITION_LEFT
        } else {
            DRIVER_POSITION_RIGHT
        }),
        can_play_native_media_during_vr: Some(true),
        display_name: s("LEHU"),
        probe_for_support: Some(false),
        connection_configuration: Some(ConnectionConfiguration {
            ping_configuration: Some(PingConfiguration {
                timeout_ms: Some(PING_TIMEOUT_MS as u32),
                interval_ms: Some(PING_INTERVAL_MS as u32),
                high_latency_threshold_ms: Some(500),
                tracked_ping_count: Some(5),
            }),
        }),
        headunit_info: Some(HeadUnitInfo {
            make: s("LEHU"),
            model: s("F1C200s"),
            year: None,
            vehicle_id: None,
            head_unit_make: s("LEHU"),
            head_unit_model: s("F1C200s"),
            head_unit_software_build: s("1"),
            head_unit_software_version: s("1.0"),
        }),
    }
}

/// The audio focus state granted for a request type: a release is a loss,
/// transient gains are granted as such, everything else is a full gain.
pub fn audio_focus_state(request_type: i32) -> i32 {
    match request_type {
        AUDIO_FOCUS_RELEASE => AUDIO_FOCUS_STATE_LOSS,
        AUDIO_FOCUS_GAIN_TRANSIENT => AUDIO_FOCUS_STATE_GAIN_TRANSIENT,
        AUDIO_FOCUS_GAIN_TRANSIENT_MAY_DUCK => AUDIO_FOCUS_STATE_GAIN_TRANSIENT_GUIDANCE_ONLY,
        _ => AUDIO_FOCUS_STATE_GAIN,
    }
}

/// The phone's ping wishes out of the version response's trailing bytes:
/// (interval, timeout) in milliseconds when both are there.
pub fn ping_config(options: &[u8]) -> Option<(u64, u64)> {
    let o: VersionResponseOptions = decode(options)?;
    let p = o.connection_configuration?.ping_configuration?;
    Some((u64::from(p.interval_ms?), u64::from(p.timeout_ms?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bytes_field_is_found_in_place_and_a_bad_walk_refused() {
        // 1: varint, 2: "ab", 3: fixed64, 4: "xyz", 5: fixed32, 4 again.
        let mut body = vec![0x08, 0x96, 0x01, 0x12, 2, b'a', b'b', 0x19];
        body.extend_from_slice(&[0; 8]);
        body.extend_from_slice(&[0x22, 3, b'x', b'y', b'z', 0x2d, 0, 0, 0, 0]);
        assert_eq!(bytes_field(&body, 4), Some(Some(&b"xyz"[..])));
        assert_eq!(bytes_field(&body, 2), Some(Some(&b"ab"[..])));
        assert_eq!(bytes_field(&body, 9), Some(None));
        body.extend_from_slice(&[0x22, 1, b'q']);
        assert_eq!(
            bytes_field(&body, 4),
            Some(Some(&b"q"[..])),
            "the last wins"
        );
        assert_eq!(bytes_field(&[], 4), Some(None));
        // A length past the end, a varint that never ends, a group.
        assert_eq!(bytes_field(&[0x22, 5, 1], 4), None);
        assert_eq!(
            bytes_field(
                &[
                    0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01
                ],
                4
            ),
            None
        );
        assert_eq!(bytes_field(&[0x0b], 4), None);
        assert_eq!(bytes_field(&[0x22, 0xff, 0xff, 0xff, 0xff, 0x0f], 4), None);
    }

    fn offer() -> Offer {
        Offer {
            resolution: 1,
            codecs: [Some(Codec::H264), None],
            fps: 30,
            dpi: 120,
            left_hand_drive: true,
            max_unacked: 4,
        }
    }

    #[test]
    fn discovery_round_trips_and_fits_an_outgoing_slot() {
        let sdr = service_discovery_response(&offer());
        let mut buf = [0u8; 512];
        let n = encode_into(&sdr, &mut buf).expect("fits");
        assert!(n > 100 && n < 512, "{n} bytes");
        let back: ServiceDiscoveryResponse = decode(&buf[..n]).expect("decodes");
        assert_eq!(back, sdr);
        let ids: Vec<i32> = back.channels.iter().map(|c| c.id).collect();
        assert_eq!(ids, [1, 3, 4, 5, 6, 9, 8, 12, 13]);
        let nav = back.channels[7].navigation_status_service.as_ref().unwrap();
        assert_eq!((nav.minimum_interval_ms, nav.cluster_type), (1000, 2));
        assert!(back.channels[8].media_playback_service.is_some());
        let touch = &back.channels[6]
            .input_source_service
            .as_ref()
            .unwrap()
            .touchscreen[0];
        assert_eq!((touch.width, touch.height), (800, 480));
        assert_eq!(
            back.channels[6]
                .input_source_service
                .as_ref()
                .unwrap()
                .keycodes_supported
                .len(),
            14
        );
        let big = Offer {
            resolution: 3,
            fps: 60,
            left_hand_drive: false,
            ..offer()
        };
        let sdr = service_discovery_response(&big);
        let video = &sdr.channels[1]
            .media_sink_service
            .as_ref()
            .unwrap()
            .video_configs[0];
        assert_eq!(video.codec_resolution, Some(3));
        assert_eq!(video.frame_rate, Some(FPS_60));
        assert_eq!(sdr.driver_position, Some(DRIVER_POSITION_RIGHT));
        assert_eq!(big.size(), (1920, 1080));
        assert_eq!(
            Offer {
                resolution: 2,
                ..offer()
            }
            .size(),
            (1280, 720)
        );
        assert_eq!(
            Offer {
                resolution: 9,
                ..offer()
            }
            .size(),
            (800, 480)
        );
    }

    fn decoders(list: &[(lehu_video::Format, Option<(u32, u32)>)]) -> Caps {
        let mut caps = Caps::none();
        for &(format, max) in list {
            caps.add_hardware(
                format,
                lehu_video::Hardware {
                    node: "/dev/video0".into(),
                    driver: "test".into(),
                    stateless: true,
                    max,
                },
            );
            caps.set_ffmpeg(format, lehu_video::Ffmpeg::all());
        }
        caps
    }

    fn video(sdr: &ServiceDiscoveryResponse) -> &MediaSinkService {
        sdr.channels[1].media_sink_service.as_ref().unwrap()
    }

    #[test]
    fn the_offer_fits_the_decoders() {
        use lehu_video::Format;
        // The F1C200s: cedrus H.264 to 1920x1088 takes every tier.
        let f1c = decoders(&[(Format::H264, Some((1920, 1088)))]);
        let mut o = Offer {
            resolution: 3,
            ..offer()
        };
        o.fit(&f1c, VideoCodec::Auto);
        assert_eq!((o.resolution, o.codecs), (3, [Some(Codec::H264), None]));
        // HEVC forced there: refused, H.264 as auto.
        o.fit(&f1c, VideoCodec::Hevc);
        assert_eq!(o.codecs, [Some(Codec::H264), None]);
        let sdr = service_discovery_response(&o);
        let v = video(&sdr);
        assert_eq!(v.available_type, Some(CODEC_VIDEO_H264_BP));
        assert_eq!(v.video_configs.len(), 1);
        assert_eq!(v.video_configs[0].codec_resolution, Some(3));

        // A decoder to 720p lowers the setting.
        let small = decoders(&[(Format::H264, Some((1280, 720)))]);
        let mut o = Offer {
            resolution: 3,
            ..offer()
        };
        o.fit(&small, VideoCodec::Auto);
        assert_eq!(o.resolution, 2);
        assert_eq!(o.size(), (1280, 720));
        // The setting is the ceiling: never raised.
        let mut o = offer();
        o.fit(&f1c, VideoCodec::Auto);
        assert_eq!(o.resolution, 1);

        // HEVC only (a Pi 5): H.265 offered first, H.264 kept last.
        let pi5 = decoders(&[(Format::Hevc, Some((4096, 4096)))]);
        let mut o = Offer {
            resolution: 2,
            ..offer()
        };
        o.fit(&pi5, VideoCodec::Auto);
        assert_eq!(o.codecs, [Some(Codec::Hevc), Some(Codec::H264)]);
        assert_eq!(o.resolution, 2);
        let sdr = service_discovery_response(&o);
        let v = video(&sdr);
        assert_eq!(v.available_type, Some(CODEC_VIDEO_H265));
        let types: Vec<_> = v.video_configs.iter().map(|c| c.video_codec_type).collect();
        assert_eq!(types, [Some(CODEC_VIDEO_H265), Some(CODEC_VIDEO_H264_BP)]);
        assert!(
            v.video_configs
                .iter()
                .all(|c| c.codec_resolution == Some(2))
        );
        assert_eq!(
            (o.config_index(7), o.config_index(3), o.config_index(5)),
            (0, 1, 0)
        );
        let mut buf = [0u8; 1024];
        let n = encode_into(&sdr, &mut buf).expect("fits");
        assert!(n < 600, "{n} bytes");

        // Both decode: H.264 first; a forced codec alone.
        let both = decoders(&[
            (Format::H264, None),
            (Format::Hevc, None),
            (Format::Vp9, None),
        ]);
        let mut o = offer();
        o.fit(&both, VideoCodec::Auto);
        assert_eq!(o.codecs, [Some(Codec::H264), Some(Codec::Hevc)]);
        o.fit(&both, VideoCodec::Hevc);
        assert_eq!(o.codecs, [Some(Codec::Hevc), None]);

        // Nothing decodes: H.264, as before, at the setting.
        let mut o = Offer {
            resolution: 3,
            ..offer()
        };
        o.fit(&Caps::none(), VideoCodec::Auto);
        assert_eq!((o.resolution, o.codecs), (3, [Some(Codec::H264), None]));
    }

    #[test]
    fn a_body_too_big_for_its_buffer_is_refused() {
        let sdr = service_discovery_response(&offer());
        let mut small = [0u8; 16];
        assert_eq!(encode_into(&sdr, &mut small), Err(TooBig));
    }

    #[test]
    fn the_small_bodies_round_trip() {
        let mut buf = [0u8; 64];
        let n = encode_into(
            &MediaAck {
                session_id: 7,
                ack: Some(1),
            },
            &mut buf,
        )
        .unwrap();
        assert_eq!(&buf[..n], &[0x08, 0x07, 0x10, 0x01]);
        let start: MediaStart = decode(&[0x08, 0x05, 0x10, 0x00]).unwrap();
        assert_eq!(start.session_id, 5);
        // prost does not enforce proto2's `required`: a missing field reads
        // as zero, and the machine lives with that.
        assert_eq!(
            decode::<MediaStart>(&[0x10, 0x00]).map(|s| s.session_id),
            Some(0)
        );
        assert!(decode::<MediaStart>(&[0x08]).is_none(), "a torn varint");
        let req: VideoFocusRequestNotification = decode(&[0x10, 0x02]).unwrap();
        assert_eq!(req.mode, Some(VIDEO_FOCUS_NATIVE));
        let open: ChannelOpenRequest = decode(&[0x08, 0x00, 0x10, 0x03]).unwrap();
        assert_eq!(open.service_id, 3);
        let n = encode_into(
            &SensorBatch {
                night_mode_data: vec![NightModeData {
                    night_mode: Some(true),
                }],
                driving_status_data: vec![],
            },
            &mut buf,
        )
        .unwrap();
        assert_eq!(&buf[..n], &[0x52, 0x02, 0x08, 0x01]);
        let n = encode_into(
            &SensorBatch {
                night_mode_data: vec![],
                driving_status_data: vec![DrivingStatusData { status: 0 }],
            },
            &mut buf,
        )
        .unwrap();
        assert_eq!(&buf[..n], &[0x6a, 0x02, 0x08, 0x00]);
        let n = encode_into(
            &MediaConfig {
                status: CONFIG_STATUS_READY,
                max_unacked: Some(4),
                configuration_indices: vec![0],
            },
            &mut buf,
        )
        .unwrap();
        assert_eq!(&buf[..n], &[0x08, 0x02, 0x10, 0x04, 0x18, 0x00]);
        let name: ServiceDiscoveryRequest = decode(&[
            0x0a, 0x02, 1, 2, 0x2a, 0x05, b'P', b'i', b'x', b'e', b'l', 0x32, 0x04, 0x0a, 0x02,
            b'i', b'd',
        ])
        .unwrap();
        assert_eq!(name.device_name.as_deref(), Some("Pixel"));
        assert_eq!(name.phone_info.unwrap().instance_id.as_deref(), Some("id"));
        let n = encode_into(
            &InputReport {
                timestamp: 1,
                touch_event: None,
                key_event: None,
                relative_event: Some(RelativeEvent {
                    data: vec![Rel {
                        keycode: KEY_ROTARY_CONTROLLER,
                        delta: -1,
                    }],
                }),
            },
            &mut buf,
        )
        .unwrap();
        let back: InputReport = decode(&buf[..n]).unwrap();
        assert_eq!(back.relative_event.unwrap().data[0].delta, -1);
    }

    #[test]
    fn focus_states_and_ping_wishes() {
        assert_eq!(audio_focus_state(AUDIO_FOCUS_GAIN), AUDIO_FOCUS_STATE_GAIN);
        assert_eq!(
            audio_focus_state(AUDIO_FOCUS_GAIN_TRANSIENT),
            AUDIO_FOCUS_STATE_GAIN_TRANSIENT
        );
        assert_eq!(
            audio_focus_state(AUDIO_FOCUS_GAIN_TRANSIENT_MAY_DUCK),
            AUDIO_FOCUS_STATE_GAIN_TRANSIENT_GUIDANCE_ONLY
        );
        assert_eq!(
            audio_focus_state(AUDIO_FOCUS_RELEASE),
            AUDIO_FOCUS_STATE_LOSS
        );
        assert_eq!(audio_focus_state(99), AUDIO_FOCUS_STATE_GAIN);
        // ConnectionConfiguration { PingConfiguration { timeout 3000, interval 1000 } }
        let ping = [0x08, 0xb8, 0x17, 0x10, 0xe8, 0x07];
        let mut options = vec![0x0a, ping.len() as u8 + 2, 0x0a, ping.len() as u8];
        options.extend_from_slice(&ping);
        assert_eq!(ping_config(&options), Some((1000, 3000)));
        assert_eq!(ping_config(&[]), None);
        assert_eq!(ping_config(&[0x0a, 0x02, 0x0a, 0x00]), None);
        assert_eq!(ping_config(&[0xff]), None);
    }
}
