// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The GAL wire vocabulary: ids, flags, channels, keycodes and the timings.
//! The numbers mirror the protobuf enums in the vendored definitions
//! (ControlMessageType, MediaMessageId, InputMessageId, SensorMessageId);
//! the short names keep the protocol code readable.

// --- The accessory switch (AOAP) ---
pub const AOAP_REQ_GET_PROTOCOL: u8 = 51;
pub const AOAP_REQ_SEND_STRING: u8 = 52;
pub const AOAP_REQ_START: u8 = 53;

/// The six accessory strings, by index. The manufacturer and model pair is
/// what makes the phone start Android Auto; the rest name this project.
pub const AOAP_STRINGS: [(u16, &str); 6] = [
    (0, "Android"),
    (1, "Android Auto"),
    (2, "LEHU head unit"),
    (3, "1.0"),
    (4, "https://github.com/mbt28/lehu"),
    (5, "LEHU"),
];

// --- Frame header flags (byte 1) ---
pub const FRAME_FIRST: u8 = 0x01;
pub const FRAME_LAST: u8 = 0x02;
pub const FRAME_CONTROL: u8 = 0x04;
pub const FRAME_ENCRYPTED: u8 = 0x08;
/// Any other bit in the flags byte means the stream is out of step.
pub const FRAME_FLAG_MASK: u8 = 0x0f;

/// The most payload one frame carries on the sending side. The receive path
/// only enforces the reassembled-message cap.
pub const MAX_FRAME_PAYLOAD: usize = 0x4000;
/// A reassembled message is never larger than this; a bigger announced or
/// accumulated size is a desync and ends the link.
pub const MAX_MESSAGE_SIZE: usize = 2 * 1024 * 1024;
/// What the reassembly buffer of a channel is grown to at most from the
/// announced total: the total is a wire field, and eight channels reserving
/// 2 MB each from eight malformed first frames would pin 16 MB.
pub const ASSEMBLY_RESERVE: usize = 256 * 1024;

// --- Channel ids, assigned by our service discovery response ---
pub const CH_CONTROL: u8 = 0;
pub const CH_SENSOR: u8 = 1;
pub const CH_VIDEO: u8 = 3;
pub const CH_MEDIA_AUDIO: u8 = 4;
pub const CH_SPEECH_AUDIO: u8 = 5;
pub const CH_SYSTEM_AUDIO: u8 = 6;
pub const CH_INPUT: u8 = 8;
pub const CH_MIC: u8 = 9;
/// Turn-by-turn and now playing, which the phone sends as text.
pub const CH_NAVIGATION: u8 = 12;
pub const CH_MEDIA_INFO: u8 = 13;
pub const CH_COUNT: usize = 16;

/// The TCP port a wireless phone connects to.
pub const TCP_PORT: u16 = 5277;

// --- The version handshake (raw, not protobuf) ---
pub const VERSION_MAJOR: u16 = 1;
pub const VERSION_MINOR: u16 = 7;
pub const VERSION_STATUS_MATCH: u16 = 0;

// --- Control channel message ids ---
pub const MSG_VERSION_REQUEST: u16 = 0x0001;
pub const MSG_VERSION_RESPONSE: u16 = 0x0002;
pub const MSG_SSL_HANDSHAKE: u16 = 0x0003;
pub const MSG_AUTH_COMPLETE: u16 = 0x0004;
pub const MSG_SERVICE_DISCOVERY_REQUEST: u16 = 0x0005;
pub const MSG_SERVICE_DISCOVERY_RESPONSE: u16 = 0x0006;
pub const MSG_CHANNEL_OPEN_REQUEST: u16 = 0x0007;
pub const MSG_CHANNEL_OPEN_RESPONSE: u16 = 0x0008;
pub const MSG_CHANNEL_CLOSE_NOTIFICATION: u16 = 0x0009;
pub const MSG_PING_REQUEST: u16 = 0x000b;
pub const MSG_PING_RESPONSE: u16 = 0x000c;
pub const MSG_NAV_FOCUS_REQUEST: u16 = 0x000d;
pub const MSG_NAV_FOCUS_NOTIFICATION: u16 = 0x000e;
pub const MSG_BYEBYE_REQUEST: u16 = 0x000f;
pub const MSG_BYEBYE_RESPONSE: u16 = 0x0010;
pub const MSG_VOICE_SESSION_NOTIFICATION: u16 = 0x0011;
pub const MSG_AUDIO_FOCUS_REQUEST: u16 = 0x0012;
pub const MSG_AUDIO_FOCUS_NOTIFICATION: u16 = 0x0013;

// --- Media channel message ids ---
/// u64BE timestamp, then the elementary stream or the PCM.
pub const MSG_MEDIA_DATA: u16 = 0x0000;
/// The H.264 parameter sets, no timestamp.
pub const MSG_MEDIA_CODEC_CONFIG: u16 = 0x0001;
pub const MSG_MEDIA_SETUP: u16 = 0x8000;
pub const MSG_MEDIA_START: u16 = 0x8001;
pub const MSG_MEDIA_STOP: u16 = 0x8002;
pub const MSG_MEDIA_CONFIG: u16 = 0x8003;
pub const MSG_MEDIA_ACK: u16 = 0x8004;
pub const MSG_MICROPHONE_REQUEST: u16 = 0x8005;
pub const MSG_MICROPHONE_RESPONSE: u16 = 0x8006;
pub const MSG_VIDEO_FOCUS_REQUEST: u16 = 0x8007;
pub const MSG_VIDEO_FOCUS_NOTIFICATION: u16 = 0x8008;

// --- Input channel message ids ---
pub const MSG_INPUT_REPORT: u16 = 0x8001;
pub const MSG_KEY_BINDING_REQUEST: u16 = 0x8002;
pub const MSG_KEY_BINDING_RESPONSE: u16 = 0x8003;

// --- Sensor channel message ids ---
pub const MSG_SENSOR_REQUEST: u16 = 0x8001;
pub const MSG_SENSOR_RESPONSE: u16 = 0x8002;
pub const MSG_SENSOR_BATCH: u16 = 0x8003;

// --- Enum values used in the bodies ---
pub const CODEC_AUDIO_PCM: i32 = 1;
pub const CODEC_VIDEO_H264_BP: i32 = 3;
pub const CODEC_VIDEO_VP9: i32 = 5;
pub const CODEC_VIDEO_AV1: i32 = 6;
pub const CODEC_VIDEO_H265: i32 = 7;
pub const AUDIO_STREAM_GUIDANCE: i32 = 1;
pub const AUDIO_STREAM_SYSTEM: i32 = 2;
pub const AUDIO_STREAM_MEDIA: i32 = 3;
pub const FPS_60: i32 = 1;
pub const FPS_30: i32 = 2;
pub const SENSOR_NIGHT_MODE: i32 = 10;
pub const SENSOR_DRIVING_STATUS: i32 = 13;
pub const DRIVER_POSITION_LEFT: i32 = 0;
pub const DRIVER_POSITION_RIGHT: i32 = 1;
pub const CONFIG_STATUS_READY: i32 = 2;
pub const STATUS_SUCCESS: i32 = 0;
pub const AUDIO_FOCUS_GAIN: i32 = 1;
pub const AUDIO_FOCUS_GAIN_TRANSIENT: i32 = 2;
pub const AUDIO_FOCUS_GAIN_TRANSIENT_MAY_DUCK: i32 = 3;
pub const AUDIO_FOCUS_RELEASE: i32 = 4;
pub const AUDIO_FOCUS_STATE_GAIN: i32 = 1;
pub const AUDIO_FOCUS_STATE_GAIN_TRANSIENT: i32 = 2;
pub const AUDIO_FOCUS_STATE_LOSS: i32 = 3;
pub const AUDIO_FOCUS_STATE_GAIN_TRANSIENT_GUIDANCE_ONLY: i32 = 7;
pub const NAV_FOCUS_NATIVE: i32 = 1;
pub const NAV_FOCUS_PROJECTED: i32 = 2;
pub const VIDEO_FOCUS_PROJECTED: i32 = 1;
pub const VIDEO_FOCUS_NATIVE: i32 = 2;
pub const VIDEO_FOCUS_NATIVE_TRANSIENT: i32 = 3;
pub const VOICE_SESSION_START: i32 = 1;
pub const VOICE_SESSION_END: i32 = 2;

// --- Android keycodes the head unit sends ---
pub const KEY_HOME: u32 = 3;
pub const KEY_BACK: u32 = 4;
pub const KEY_DPAD_UP: u32 = 19;
pub const KEY_DPAD_DOWN: u32 = 20;
pub const KEY_DPAD_LEFT: u32 = 21;
pub const KEY_DPAD_RIGHT: u32 = 22;
pub const KEY_DPAD_CENTER: u32 = 23;
pub const KEY_SEARCH: u32 = 84;
pub const KEY_PLAY_PAUSE: u32 = 85;
pub const KEY_MEDIA_NEXT: u32 = 87;
pub const KEY_MEDIA_PREVIOUS: u32 = 88;
pub const KEY_MEDIA_PLAY: u32 = 126;
pub const KEY_MEDIA_PAUSE: u32 = 127;
/// A rotary controller's turns: relative events, +1 per clockwise detent.
pub const KEY_ROTARY_CONTROLLER: u32 = 65536;

/// Every keycode declared to the phone, in the order of the declaration.
pub const KEYCODES: [i32; 14] = [
    KEY_HOME as i32,
    KEY_BACK as i32,
    KEY_DPAD_UP as i32,
    KEY_DPAD_DOWN as i32,
    KEY_DPAD_LEFT as i32,
    KEY_DPAD_RIGHT as i32,
    KEY_DPAD_CENTER as i32,
    KEY_SEARCH as i32,
    KEY_PLAY_PAUSE as i32,
    KEY_MEDIA_NEXT as i32,
    KEY_MEDIA_PREVIOUS as i32,
    KEY_MEDIA_PLAY as i32,
    KEY_MEDIA_PAUSE as i32,
    KEY_ROTARY_CONTROLLER as i32,
];

// --- Touch actions (PointerAction) ---
pub const TOUCH_DOWN: i32 = 0;
pub const TOUCH_UP: i32 = 1;
pub const TOUCH_MOVED: i32 = 2;
pub const TOUCH_POINTER_DOWN: i32 = 5;
pub const TOUCH_POINTER_UP: i32 = 6;

// --- Timings, in milliseconds ---
/// How often we ping when the phone names no interval of its own (the
/// cadence our discovery response asks of the phone as well).
pub const PING_INTERVAL_MS: u64 = 1500;
/// A ping unanswered this long means the phone is gone.
pub const PING_TIMEOUT_MS: u64 = 5000;
/// A session that is not showing video this long after the link came up
/// is stuck (a zombie Android Auto service on the phone) and is ended.
pub const SETUP_TIMEOUT_MS: u64 = 30_000;
/// After our ByeBye, how long the phone has to acknowledge before the link
/// is closed anyway.
pub const BYEBYE_WAIT_MS: u64 = 1000;
/// With `keyframe_by_focus`, the gap between taking the focus away and
/// giving it back, which makes the phone restart its stream on a keyframe.
pub const KEYFRAME_TOGGLE_MS: u64 = 60;
/// Such restarts are rate limited: each costs a stream restart.
pub const KEYFRAME_TOGGLE_MIN_GAP_MS: u64 = 1000;
