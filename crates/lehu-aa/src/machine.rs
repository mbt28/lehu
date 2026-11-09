// SPDX-License-Identifier: GPL-3.0-or-later
//! The GAL session as a state machine, transport-agnostic and driven by
//! the session's threads: the process thread feeds it frames, the write
//! thread feeds it the loop's commands and the clock, and both take what
//! it queues for the phone out of the outbox. Time is a millisecond count
//! passed in, so the tests drive the timers.
//!
//! The handshake in order: our version request, the phone's version
//! response (with its ping wishes), the TLS handshake carried in plaintext
//! frames, our auth-complete, the phone's service discovery request, our
//! response with the head unit's capabilities, the phone's channel opens,
//! then the media setups and starts. Video is Ready at the video setup and
//! Live at its start.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use lehu_core::queue::Queue;
use lehu_core::settings::{NightMode, Settings};
use lehu_input::{Contact, MAX_SLOTS, PhoneButton, TOUCH_SCALE};
use lehu_session::{
    AccessUnit, AudioCodec, AudioKind, Cmd, Codec, Event, Fault, MicFormat, Phase, Ports, Sinks,
};
use lehu_usb::Id;

use crate::consts::*;
use crate::frame::{Assembler, Class, Desync, Header};
use crate::proto::{self, Offer};
use crate::tls::Tls;
use crate::transport::UsbTuning;

/// The body an outgoing message can carry: the TLS handshake's client
/// flight (certificate, key exchange, finished) is the largest at about
/// 1.3 KB, the discovery response the next.
pub const OUT_BODY: usize = 2048;

/// What the write thread works through: a handful a second, plus a touch
/// frame per report. Over this the write thread is stuck.
pub const OUTBOX_DEPTH: usize = 32;

/// Access units are reused through `ports.recycle`; one is made this big
/// so a keyframe at the phone's resolution fits, and bigger only for the
/// rare unit that does not.
pub const UNIT_BYTES: usize = 256 * 1024;

/// How to answer the phone's request for native (the head unit's own)
/// video focus, its exit button. The two sources disagree; which is right
/// is settled by a test on the board.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeAnswer {
    /// FastCarPlay: honour it, the phone stops its video and the session
    /// stays for a resume. Answering "projected" to a request to hand the
    /// screen back left the phone with nowhere to go, and it escalated to
    /// a shutdown.
    Native,
    /// LIVI: answer "projected" regardless and only tell the loop.
    Projected,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub offer: Offer,
    pub force_chacha20: bool,
    pub night: NightMode,
    pub native_answer: NativeAnswer,
    /// Serve a keyframe request by taking the video focus away and giving
    /// it back 60 ms later, which makes the phone restart its stream on a
    /// keyframe. Off: the decoder decodes through the gap, as FastCarPlay
    /// did; the loop should then not ask for keyframes.
    pub keyframe_by_focus: bool,
    pub usb: UsbTuning,
    /// 0: any phone. Both set: only that one.
    pub vendor_id: u32,
    pub product_id: u32,
}

impl Config {
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            // H.264 alone until `Offer::fit` has the system's decoders.
            offer: Offer {
                resolution: s.android_auto.resolution,
                codecs: [Some(Codec::H264), None],
                fps: s.android_auto.fps,
                dpi: s.android_auto.dpi,
                left_hand_drive: s.display.left_hand_drive,
                max_unacked: s.android_auto.max_unacked,
            },
            force_chacha20: s.android_auto.force_chacha20,
            night: s.display.night_mode,
            native_answer: NativeAnswer::Native,
            keyframe_by_focus: false,
            usb: UsbTuning::from_settings(&s.usb),
            vendor_id: s.android_auto.vendor_id,
            product_id: s.android_auto.product_id,
        }
    }

    /// Whether a device may be switched: any, unless one phone is pinned.
    pub fn accepts(&self, id: Id) -> bool {
        (self.vendor_id == 0 || self.vendor_id == u32::from(id.vendor))
            && (self.product_id == 0 || self.product_id == u32::from(id.product))
    }
}

/// One message for the phone, fixed size so the outbox never allocates.
#[derive(Clone, Copy)]
pub struct OutMsg {
    pub channel: u8,
    pub class: Class,
    pub msg_id: u16,
    len: u16,
    body: [u8; OUT_BODY],
}

impl OutMsg {
    pub fn body(&self) -> &[u8] {
        &self.body[..usize::from(self.len)]
    }
}

/// What the write thread takes out of the outbox, in order. One size for
/// every item, so the queue holds them by value and never allocates.
#[derive(Clone, Copy)]
#[allow(clippy::large_enum_variant)]
pub enum Work {
    Msg(OutMsg),
    /// A command from the loop, translated on the write thread.
    Cmd(Cmd),
    /// Close the link once everything queued before it was written.
    Close,
}

/// Why the process thread stops reading.
#[derive(Debug)]
pub enum Fatal {
    Desync(Desync),
    Tls(String),
    /// The phone's version does not match ours, or the TLS handshake
    /// failed.
    Rejected(String),
}

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fatal::Desync(d) => write!(f, "out of step: {d}"),
            Fatal::Tls(e) => write!(f, "tls: {e}"),
            Fatal::Rejected(e) => f.write_str(e),
        }
    }
}

impl Fatal {
    pub fn fault(&self) -> Fault {
        match self {
            Fatal::Desync(_) | Fatal::Tls(_) => Fault::LinkLost,
            Fatal::Rejected(_) => Fault::Rejected,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct ChannelState {
    open: bool,
    session: i32,
}

/// Nanoseconds of the monotonic clock, the timestamps the phone is given
/// on input reports, pings and microphone frames.
pub fn now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a plain clock read into a local.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64) * 1_000_000_000 + ts.tv_nsec as u64
}

pub fn phase_from_u8(v: u8) -> Phase {
    match v {
        0 => Phase::Spawned,
        1 => Phase::Identified,
        2 => Phase::Ready,
        3 => Phase::Live,
        4 => Phase::Backgrounded,
        5 => Phase::Closing,
        _ => Phase::Closed,
    }
}

pub struct Machine {
    config: Config,
    tls: Tls,
    assembler: Assembler,
    outbox: Arc<Queue<Work>>,
    ports: Ports,
    sinks: Box<dyn Sinks>,
    /// The phase as the loop reads it between events.
    phase_mirror: Arc<AtomicU8>,
    name: Arc<OnceLock<String>>,
    /// The phone's instance id from its discovery request.
    identity: Arc<OnceLock<String>>,
    phase: Phase,
    auth: bool,
    channels: [ChannelState; CH_COUNT],
    /// The phone holds the screen as far as it has been told.
    focused: bool,
    night: bool,
    live_once: bool,
    ready_reported: bool,
    mic_open: bool,
    mic_session: i32,
    ending: bool,
    started: u64,
    ping_interval: u64,
    ping_timeout: u64,
    /// When the next ping goes; 0 until TLS is up.
    next_ping: u64,
    /// The last sign of life, and when the first ping since it went out
    /// (0: none outstanding), for the timeout.
    last_heard: u64,
    unanswered_since: u64,
    /// After our ByeBye: when to close without the phone's answer.
    close_by: u64,
    /// With `keyframe_by_focus`: when to give the focus back.
    toggle_at: u64,
    last_toggle: u64,
    pcm: Vec<i16>,
    scratch: Vec<u8>,
}

impl Machine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        tls: Tls,
        outbox: Arc<Queue<Work>>,
        ports: Ports,
        sinks: Box<dyn Sinks>,
        phase_mirror: Arc<AtomicU8>,
        name: Arc<OnceLock<String>>,
        identity: Arc<OnceLock<String>>,
        now: u64,
    ) -> Self {
        let night = config.night == NightMode::Night;
        Self {
            config,
            tls,
            assembler: Assembler::new(),
            outbox,
            ports,
            sinks,
            phase_mirror,
            name,
            identity,
            phase: Phase::Spawned,
            auth: false,
            channels: [ChannelState::default(); CH_COUNT],
            focused: true,
            night,
            live_once: false,
            ready_reported: false,
            mic_open: false,
            mic_session: 1,
            ending: false,
            started: now,
            ping_interval: PING_INTERVAL_MS,
            ping_timeout: PING_TIMEOUT_MS,
            next_ping: 0,
            last_heard: now,
            unanswered_since: 0,
            close_by: 0,
            toggle_at: 0,
            last_toggle: 0,
            pcm: Vec::new(),
            scratch: Vec::with_capacity(OUT_BODY),
        }
    }

    /// Open the conversation: the version request, a raw pair of u16BE.
    pub fn start(&mut self) {
        let v = [
            (VERSION_MAJOR >> 8) as u8,
            VERSION_MAJOR as u8,
            (VERSION_MINOR >> 8) as u8,
            VERSION_MINOR as u8,
        ];
        self.send_raw(CH_CONTROL, Class::Plaintext, MSG_VERSION_REQUEST, &v);
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn mic_open(&self) -> bool {
        self.mic_open
    }

    pub fn ending(&self) -> bool {
        self.ending
    }

    pub fn cipher_name(&self) -> &'static str {
        self.tls.cipher_name()
    }

    /// Plaintext in, TLS records appended to `out`; the write thread's.
    pub fn encrypt(&mut self, plain: &[u8], out: &mut Vec<u8>) -> Result<(), crate::tls::Error> {
        self.tls.encrypt(plain, out)
    }

    // --- Reporting ---

    fn report(&self, event: Event) {
        self.ports.report(event);
    }

    fn set_phase(&mut self, phase: Phase) {
        if self.phase == phase || self.phase == Phase::Closed {
            return;
        }
        self.phase = phase;
        self.phase_mirror.store(phase as u8, Ordering::Release);
        self.report(Event::Phase(phase));
    }

    /// The session is over: the fault, if any and not of our own making,
    /// then Closed, once.
    pub fn finish(&mut self, fault: Option<Fault>) {
        if self.phase == Phase::Closed {
            return;
        }
        if let Some(f) = fault
            && !self.ending
        {
            self.report(Event::Fault(f));
        }
        if self.mic_open {
            self.mic_open = false;
            self.sinks.mic(None);
        }
        self.ending = true;
        self.set_phase(Phase::Closed);
    }

    /// End the link ourselves, with a fault for the loop.
    fn fail(&mut self, fault: Fault) {
        if self.ending {
            return;
        }
        self.report(Event::Fault(fault));
        self.ending = true;
        self.outbox.push_drop_oldest(Work::Close);
    }

    // --- Sending ---

    fn send_raw(&mut self, channel: u8, class: Class, msg_id: u16, body: &[u8]) {
        if body.len() > OUT_BODY {
            log::error!(
                "aa: message {msg_id:#06x} of {} bytes does not fit",
                body.len()
            );
            return;
        }
        let mut m = OutMsg {
            channel,
            class,
            msg_id,
            len: body.len() as u16,
            body: [0; OUT_BODY],
        };
        m.body[..body.len()].copy_from_slice(body);
        if !self.outbox.push_discard(Work::Msg(m)) {
            log::warn!("aa: outbox full, message {msg_id:#06x} dropped");
        }
    }

    fn send(&mut self, channel: u8, class: Class, msg_id: u16, m: &impl prost::Message) {
        let mut body = [0u8; OUT_BODY];
        match proto::encode_into(m, &mut body) {
            Ok(n) => self.send_raw(channel, class, msg_id, &body[..n]),
            Err(_) => log::error!("aa: message {msg_id:#06x} does not fit a slot"),
        }
    }

    fn send_signal(&mut self, channel: u8, msg_id: u16, m: &impl prost::Message) {
        self.send(channel, Class::EncSignal, msg_id, m);
    }

    // --- The receive path ---

    /// One frame from the transport: reassembled, decrypted into the
    /// channel's buffer, dispatched when complete. A video unit comes
    /// back to be pushed by the caller outside any lock.
    pub fn on_frame(
        &mut self,
        now: u64,
        header: &Header,
        total: u32,
        payload: &[u8],
    ) -> Result<Option<AccessUnit>, Fatal> {
        self.last_heard = now;
        self.unanswered_since = 0;
        let Some(buf) = self.assembler.target(header, total) else {
            log::debug!(
                "aa: continuation without a start on channel {}, dropped",
                header.channel
            );
            return Ok(None);
        };
        if header.encrypted() {
            if let Err(e) = self.tls.decrypt(payload, buf) {
                return Err(Fatal::Tls(e.0));
            }
        } else if header.channel != CH_CONTROL {
            // Only the control channel's version and handshake travel in
            // the clear; anything else unencrypted is not the phone's, and
            // must not reach the decoder or the mixer.
            log::debug!(
                "aa: an unencrypted frame on channel {}, dropped",
                header.channel
            );
            return Ok(None);
        } else {
            buf.extend_from_slice(payload);
        }
        if self
            .assembler
            .complete(header)
            .map_err(Fatal::Desync)?
            .is_none()
        {
            return Ok(None);
        }
        // The buffer is taken out so the handlers may borrow the machine;
        // it goes back, capacity and all.
        let msg = self.assembler.take_message(header.channel);
        let result = self.dispatch(now, header.channel, &msg);
        self.assembler.restore(header.channel, msg);
        result
    }

    fn dispatch(&mut self, now: u64, ch: u8, msg: &[u8]) -> Result<Option<AccessUnit>, Fatal> {
        if msg.len() < 2 {
            log::warn!("aa: short message on channel {ch} ({} bytes)", msg.len());
            return Ok(None);
        }
        let id = u16::from_be_bytes([msg[0], msg[1]]);
        let body = &msg[2..];
        log::trace!("aa: rx ch {ch} id {id:#06x} ({} bytes)", body.len());
        if id == MSG_CHANNEL_OPEN_REQUEST && ch != CH_CONTROL {
            let service = proto::decode::<proto::ChannelOpenRequest>(body)
                .map(|r| r.service_id)
                .unwrap_or(-1);
            log::info!("aa: channel {ch} open request (service {service})");
            self.channels[usize::from(ch) % CH_COUNT].open = true;
            self.send(
                ch,
                Class::EncControl,
                MSG_CHANNEL_OPEN_RESPONSE,
                &proto::ChannelOpenResponse {
                    status: STATUS_SUCCESS,
                },
            );
            if ch == CH_SENSOR {
                // The phone blocks projection until it knows the driving
                // status.
                self.send_driving_status();
            }
            return Ok(None);
        }
        match ch {
            CH_CONTROL => self.control(now, id, body)?,
            CH_VIDEO => return Ok(self.media(ch, id, body)),
            CH_MEDIA_AUDIO | CH_SPEECH_AUDIO | CH_SYSTEM_AUDIO | CH_MIC => {
                self.media(ch, id, body);
            }
            CH_SENSOR => self.sensor(id, body),
            CH_INPUT => self.input(id, body),
            CH_MEDIA_INFO => {
                if !self
                    .ports
                    .tell_now_playing(|np| crate::status::playback(id, body, np, now))
                {
                    log::debug!("aa: playback message {id:#06x} not taken");
                }
                self.ports
                    .tell_artwork(|a| crate::status::artwork(id, body, a));
            }
            CH_NAVIGATION => {
                if !self
                    .ports
                    .tell_guidance(|g| crate::status::navigation(id, body, g))
                {
                    log::debug!("aa: navigation message {id:#06x} not taken");
                }
            }
            _ => log::debug!("aa: message {id:#06x} on unhandled channel {ch}"),
        }
        Ok(None)
    }

    fn control(&mut self, now: u64, id: u16, body: &[u8]) -> Result<(), Fatal> {
        match id {
            MSG_VERSION_RESPONSE => {
                // Raw: u16BE major, minor, status; then the phone's options.
                if body.len() < 6 {
                    return Err(Fatal::Rejected("short version response".into()));
                }
                let major = u16::from_be_bytes([body[0], body[1]]);
                let minor = u16::from_be_bytes([body[2], body[3]]);
                let status = u16::from_be_bytes([body[4], body[5]]);
                if status != VERSION_STATUS_MATCH {
                    return Err(Fatal::Rejected(format!(
                        "version mismatch (phone {major}.{minor}, status {status})"
                    )));
                }
                if let Some((interval, timeout)) = proto::ping_config(&body[6..])
                    && interval > 0
                {
                    self.ping_interval = interval;
                    self.ping_timeout = timeout.max(interval);
                }
                log::info!(
                    "aa: version {major}.{minor}, ping every {} ms, starting TLS",
                    self.ping_interval
                );
                self.handshake(&[])?;
            }
            MSG_SSL_HANDSHAKE => self.handshake(body)?,
            MSG_SERVICE_DISCOVERY_REQUEST => {
                let req = proto::decode::<proto::ServiceDiscoveryRequest>(body).unwrap_or_default();
                let name = req
                    .device_name
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| "phone".to_string());
                log::info!("aa: service discovery request from '{name}'");
                let _ = self.name.set(name);
                if let Some(id) = req
                    .phone_info
                    .and_then(|p| p.instance_id)
                    .filter(|i| !i.is_empty())
                {
                    let _ = self.identity.set(id);
                }
                let sdr = proto::service_discovery_response(&self.config.offer);
                self.send_signal(CH_CONTROL, MSG_SERVICE_DISCOVERY_RESPONSE, &sdr);
                self.set_phase(Phase::Identified);
            }
            MSG_PING_REQUEST => {
                let ts = proto::decode::<proto::PingRequest>(body)
                    .map(|p| p.timestamp)
                    .unwrap_or(0);
                self.send(
                    CH_CONTROL,
                    Class::Plaintext,
                    MSG_PING_RESPONSE,
                    &proto::PingResponse { timestamp: ts },
                );
            }
            MSG_PING_RESPONSE => {}
            MSG_AUDIO_FOCUS_REQUEST => {
                let t = proto::decode::<proto::AudioFocusRequest>(body)
                    .map(|r| r.audio_focus_type)
                    .unwrap_or(0);
                log::debug!("aa: audio focus request {t}");
                self.send_signal(
                    CH_CONTROL,
                    MSG_AUDIO_FOCUS_NOTIFICATION,
                    &proto::AudioFocusNotification {
                        focus_state: proto::audio_focus_state(t),
                        unsolicited: Some(false),
                    },
                );
            }
            MSG_NAV_FOCUS_REQUEST => {
                let wanted = proto::decode::<proto::NavFocusRequestNotification>(body)
                    .and_then(|r| r.focus_type)
                    .unwrap_or(NAV_FOCUS_PROJECTED);
                self.send_signal(
                    CH_CONTROL,
                    MSG_NAV_FOCUS_NOTIFICATION,
                    &proto::NavFocusNotification {
                        focus_type: NAV_FOCUS_PROJECTED,
                    },
                );
                self.report(Event::NavFocus(wanted != NAV_FOCUS_NATIVE));
            }
            MSG_BYEBYE_REQUEST => {
                // A genuine end of the session (unplug, or Android Auto
                // stopped), not the exit button: that is a focus request.
                let reason = proto::decode::<proto::ByeByeRequest>(body)
                    .map(|r| r.reason)
                    .unwrap_or(0);
                log::info!("aa: the phone ends the session (reason {reason})");
                self.send_signal(CH_CONTROL, MSG_BYEBYE_RESPONSE, &proto::ByeByeResponse {});
                self.report(Event::Fault(Fault::PhoneLeft));
                self.ending = true;
                self.set_phase(Phase::Closing);
                self.outbox.push_drop_oldest(Work::Close);
            }
            MSG_BYEBYE_RESPONSE => {
                if self.close_by != 0 {
                    log::info!("aa: goodbye acknowledged");
                    self.close_by = 0;
                    self.outbox.push_drop_oldest(Work::Close);
                }
            }
            MSG_VOICE_SESSION_NOTIFICATION => {
                let status = proto::decode::<proto::VoiceSessionNotification>(body)
                    .and_then(|v| v.status)
                    .unwrap_or(0);
                self.report(Event::Assistant(status == VOICE_SESSION_START));
            }
            MSG_CHANNEL_CLOSE_NOTIFICATION => log::debug!("aa: channel close notification"),
            _ => log::debug!(
                "aa: unhandled control message {id:#06x} ({} bytes)",
                body.len()
            ),
        }
        let _ = now;
        Ok(())
    }

    /// One TLS handshake step; the records go out as plaintext frames.
    fn handshake(&mut self, input: &[u8]) -> Result<(), Fatal> {
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        let done = self
            .tls
            .handshake(input, &mut out)
            .map_err(|e| Fatal::Rejected(format!("tls handshake: {e}")));
        if !out.is_empty() {
            self.send_raw(CH_CONTROL, Class::Plaintext, MSG_SSL_HANDSHAKE, &out);
        }
        self.scratch = out;
        if done? {
            log::info!("aa: tls up ({})", self.tls.cipher_name());
            self.send(
                CH_CONTROL,
                Class::Plaintext,
                MSG_AUTH_COMPLETE,
                &proto::AuthResponse {
                    status: STATUS_SUCCESS,
                },
            );
            self.auth = true;
            self.unanswered_since = 0;
            self.next_ping = self.last_heard + self.ping_interval;
        }
        Ok(())
    }

    fn media(&mut self, ch: u8, id: u16, body: &[u8]) -> Option<AccessUnit> {
        let slot = usize::from(ch) % CH_COUNT;
        match id {
            MSG_MEDIA_SETUP => {
                let codec = proto::decode::<proto::MediaSetup>(body)
                    .map(|s| s.codec_type)
                    .unwrap_or(0);
                log::info!("aa: media setup on channel {ch} (codec {codec})");
                // The configuration the phone took is the one for its codec.
                let index = if ch == CH_VIDEO {
                    self.config.offer.config_index(codec)
                } else {
                    0
                };
                self.send_signal(
                    ch,
                    MSG_MEDIA_CONFIG,
                    &proto::MediaConfig {
                        status: CONFIG_STATUS_READY,
                        max_unacked: Some(self.config.offer.max_unacked),
                        configuration_indices: vec![index],
                    },
                );
                if ch == CH_VIDEO {
                    if !self.ready_reported {
                        self.ready_reported = true;
                        let codec = proto::codec_of(codec).unwrap_or_else(|| {
                            log::warn!("aa: the phone set up codec {codec}, which was not offered");
                            Codec::H264
                        });
                        self.report(Event::VideoCodec(codec));
                        self.set_phase(Phase::Ready);
                    }
                    self.focused = true;
                    self.notify_focus(true, true);
                }
            }
            MSG_MEDIA_START => {
                let session = proto::decode::<proto::MediaStart>(body)
                    .map(|s| s.session_id)
                    .unwrap_or(0);
                self.channels[slot].session = session;
                log::info!("aa: media start on channel {ch} (session {session})");
                match ch {
                    CH_VIDEO => {
                        self.live_once = true;
                        self.set_phase(Phase::Live);
                    }
                    CH_MIC => {}
                    _ => self.report(Event::Audio(audio_kind(ch), true)),
                }
            }
            MSG_MEDIA_STOP => {
                log::info!("aa: media stop on channel {ch}");
                self.channels[slot].session = -1;
                match ch {
                    CH_VIDEO => {
                        if self.phase == Phase::Live {
                            self.set_phase(Phase::Backgrounded);
                        }
                    }
                    CH_MIC => self.set_mic(false),
                    _ => self.report(Event::Audio(audio_kind(ch), false)),
                }
            }
            MSG_VIDEO_FOCUS_REQUEST if ch == CH_VIDEO => self.focus_request(body),
            MSG_MEDIA_DATA => {
                if body.len() < 8 {
                    return None;
                }
                // u64BE timestamp, then the elementary stream or the PCM.
                let unit = match ch {
                    CH_VIDEO => self.video_unit(&body[8..], false),
                    CH_MIC => None,
                    _ => {
                        self.audio(ch, &body[8..]);
                        None
                    }
                };
                self.ack(ch);
                return unit;
            }
            MSG_MEDIA_CODEC_CONFIG => {
                // The H.264 parameter sets, no timestamp, straight to the
                // decoder.
                let unit = if ch == CH_VIDEO {
                    self.video_unit(body, true)
                } else {
                    None
                };
                self.ack(ch);
                return unit;
            }
            MSG_MICROPHONE_REQUEST if ch == CH_MIC => {
                let open = proto::decode::<proto::MicrophoneRequest>(body).is_some_and(|r| r.open);
                self.send_signal(
                    CH_MIC,
                    MSG_MICROPHONE_RESPONSE,
                    &proto::MicrophoneResponse {
                        status: STATUS_SUCCESS,
                        session_id: Some(self.mic_session),
                    },
                );
                if open && !self.mic_open {
                    self.send_signal(
                        CH_MIC,
                        MSG_MEDIA_START,
                        &proto::MediaStart {
                            session_id: self.mic_session,
                            configuration_index: 0,
                        },
                    );
                }
                self.set_mic(open);
            }
            MSG_MEDIA_ACK => {}
            _ => log::debug!("aa: unhandled media message {id:#06x} on channel {ch}"),
        }
        None
    }

    fn ack(&mut self, ch: u8) {
        let session = self.channels[usize::from(ch) % CH_COUNT].session;
        self.send_signal(
            ch,
            MSG_MEDIA_ACK,
            &proto::MediaAck {
                session_id: session,
                ack: Some(1),
            },
        );
    }

    fn set_mic(&mut self, open: bool) {
        if open == self.mic_open {
            return;
        }
        self.mic_open = open;
        log::info!("aa: microphone {}", if open { "open" } else { "closed" });
        self.sinks.mic(open.then_some(MicFormat {
            codec: AudioCodec::Pcm,
            rate: 16000,
            frame_tenths_ms: 200,
        }));
    }

    /// An access unit for the decoder, from the recycle queue when one
    /// there is big enough.
    fn video_unit(&mut self, data: &[u8], config: bool) -> Option<AccessUnit> {
        if data.is_empty() {
            return None;
        }
        let mut unit = match self.ports.recycle.pop() {
            Some(mut u) if u.capacity() >= data.len() => {
                u.clear();
                u
            }
            _ => AccessUnit::with_capacity(data.len().max(UNIT_BYTES)),
        };
        unit.write(data);
        unit.config = config;
        Some(unit)
    }

    /// PCM bytes to the sink as samples, through a buffer kept for it.
    fn audio(&mut self, ch: u8, bytes: &[u8]) {
        let (kind, rate, channels) = match ch {
            CH_MEDIA_AUDIO => (AudioKind::Media, 48000, 2),
            CH_SPEECH_AUDIO => (AudioKind::Speech, 16000, 1),
            _ => (AudioKind::System, 16000, 1),
        };
        self.pcm.clear();
        self.pcm.extend(
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| i16::from_le_bytes(*p)),
        );
        if self.pcm.is_empty() {
            return;
        }
        self.sinks.audio_pcm(kind, rate, channels, &self.pcm);
    }

    /// What the phone asks for: projected is "show me"; native (and
    /// native transient) is "take the screen back", its exit button. The
    /// session stays up either way: this is focus, not shutdown.
    fn focus_request(&mut self, body: &[u8]) {
        let mode = proto::decode::<proto::VideoFocusRequestNotification>(body).and_then(|r| r.mode);
        // An unset mode means "show me"; refusing to project is the worse
        // failure.
        let projected = !matches!(
            mode,
            Some(VIDEO_FOCUS_NATIVE | VIDEO_FOCUS_NATIVE_TRANSIENT)
        );
        log::info!(
            "aa: the phone asks for {} focus",
            if projected { "projected" } else { "native" }
        );
        self.toggle_at = 0;
        if projected {
            self.focused = true;
            self.notify_focus(true, false);
            self.report(Event::VideoFocus(true));
            return;
        }
        match self.config.native_answer {
            NativeAnswer::Native => {
                self.focused = false;
                self.notify_focus(false, false);
            }
            NativeAnswer::Projected => {
                self.focused = true;
                self.notify_focus(true, false);
            }
        }
        self.report(Event::HostUi);
    }

    fn notify_focus(&mut self, projected: bool, unsolicited: bool) {
        self.send_signal(
            CH_VIDEO,
            MSG_VIDEO_FOCUS_NOTIFICATION,
            &proto::VideoFocusNotification {
                focus: Some(if projected {
                    VIDEO_FOCUS_PROJECTED
                } else {
                    VIDEO_FOCUS_NATIVE
                }),
                unsolicited: Some(unsolicited),
            },
        );
    }

    fn sensor(&mut self, id: u16, body: &[u8]) {
        if id != MSG_SENSOR_REQUEST {
            log::debug!("aa: unhandled sensor message {id:#06x}");
            return;
        }
        let kind = proto::decode::<proto::SensorRequest>(body)
            .map(|r| r.sensor_type)
            .unwrap_or(0);
        log::debug!("aa: sensor start request (type {kind})");
        self.send_signal(
            CH_SENSOR,
            MSG_SENSOR_RESPONSE,
            &proto::SensorStartResponse {
                status: STATUS_SUCCESS,
            },
        );
        // An event right away: the phone does not project until it has
        // seen the driving status.
        match kind {
            SENSOR_DRIVING_STATUS => self.send_driving_status(),
            SENSOR_NIGHT_MODE => self.send_night(),
            _ => {}
        }
    }

    fn send_driving_status(&mut self) {
        self.send_signal(
            CH_SENSOR,
            MSG_SENSOR_BATCH,
            &proto::SensorBatch {
                night_mode_data: Vec::new(),
                driving_status_data: vec![proto::DrivingStatusData { status: 0 }],
            },
        );
    }

    fn send_night(&mut self) {
        let night = self.night;
        self.send_signal(
            CH_SENSOR,
            MSG_SENSOR_BATCH,
            &proto::SensorBatch {
                night_mode_data: vec![proto::NightModeData {
                    night_mode: Some(night),
                }],
                driving_status_data: Vec::new(),
            },
        );
    }

    fn input(&mut self, id: u16, _body: &[u8]) {
        if id == MSG_KEY_BINDING_REQUEST {
            self.send_signal(
                CH_INPUT,
                MSG_KEY_BINDING_RESPONSE,
                &proto::KeyBindingResponse {
                    status: STATUS_SUCCESS,
                },
            );
            return;
        }
        log::debug!("aa: unhandled input message {id:#06x}");
    }

    // --- The loop's commands ---

    fn input_open(&self) -> bool {
        self.auth && self.channels[usize::from(CH_INPUT)].open
    }

    pub fn on_cmd(&mut self, now: u64, ts_ns: u64, cmd: Cmd) {
        match cmd {
            Cmd::Touch { count, contacts } => {
                // Before the phone opens the input channel a touch or a
                // key is dropped, not an error.
                if !self.input_open() {
                    return;
                }
                let (w, h) = self.config.offer.size();
                if let Some(report) = touch_report(ts_ns, &contacts, count, w, h) {
                    self.send_signal(CH_INPUT, MSG_INPUT_REPORT, &report);
                }
            }
            Cmd::Key { button, down } => {
                if !self.input_open() || !down {
                    // Every button is sent as a full press on its down;
                    // the loop sends no release for most of them.
                    return;
                }
                match button_action(button) {
                    Some(Action::Key(code)) => self.key_press(ts_ns, code),
                    Some(Action::Rotary(delta)) => self.rotary(ts_ns, delta),
                    None => {}
                }
            }
            Cmd::Rotary(detents) => {
                if self.input_open() && detents != 0 {
                    self.rotary(ts_ns, i32::from(detents));
                }
            }
            Cmd::Keyframe => self.keyframe(now),
            Cmd::VideoActive(on) => self.set_focus(on),
            Cmd::Resume => self.set_focus(true),
            Cmd::Night(mode) => {
                // No light sensor to follow: Auto is day, as at connect.
                self.night = mode == NightMode::Night;
                if self.channels[usize::from(CH_SENSOR)].open {
                    self.send_night();
                }
            }
            Cmd::Close => self.close(now),
        }
    }

    fn key_press(&mut self, ts_ns: u64, code: u32) {
        for down in [true, false] {
            self.send_signal(
                CH_INPUT,
                MSG_INPUT_REPORT,
                &proto::InputReport {
                    timestamp: ts_ns,
                    touch_event: None,
                    key_event: Some(proto::KeyEvent {
                        keys: vec![proto::Key {
                            keycode: code,
                            down,
                            metastate: 0,
                        }],
                    }),
                    relative_event: None,
                },
            );
        }
    }

    fn rotary(&mut self, ts_ns: u64, delta: i32) {
        self.send_signal(
            CH_INPUT,
            MSG_INPUT_REPORT,
            &proto::InputReport {
                timestamp: ts_ns,
                touch_event: None,
                key_event: None,
                relative_event: Some(proto::RelativeEvent {
                    data: vec![proto::Rel {
                        keycode: KEY_ROTARY_CONTROLLER,
                        delta,
                    }],
                }),
            },
        );
    }

    /// The loop says who has the screen: the phone is told it lost the
    /// panel (the screens, the camera) or gets it back, which restarts
    /// its stream on the parameter sets and a keyframe. Nothing is sent
    /// when the phone already knows.
    fn set_focus(&mut self, on: bool) {
        if !self.channels[usize::from(CH_VIDEO)].open || self.focused == on {
            return;
        }
        log::info!(
            "aa: {}",
            if on {
                "back to the phone's video"
            } else {
                "the head unit takes the screen"
            }
        );
        self.toggle_at = 0;
        self.focused = on;
        self.notify_focus(on, true);
    }

    /// Android Auto cannot be asked for a keyframe. With
    /// `keyframe_by_focus` the focus is taken away and given back, which
    /// restarts the stream; otherwise nothing, and the decoder decodes
    /// through the gap.
    fn keyframe(&mut self, now: u64) {
        if !self.config.keyframe_by_focus
            || !self.focused
            || self.phase != Phase::Live
            || self.toggle_at != 0
            || now < self.last_toggle + KEYFRAME_TOGGLE_MIN_GAP_MS
        {
            return;
        }
        log::debug!("aa: restarting the stream for a keyframe");
        self.notify_focus(false, true);
        self.toggle_at = now + KEYFRAME_TOGGLE_MS;
        self.last_toggle = now;
    }

    /// Goodbye: the loop is told first, then ByeBye goes and the link is
    /// closed on the phone's answer or after a second.
    fn close(&mut self, now: u64) {
        if self.ending {
            return;
        }
        self.ending = true;
        self.set_phase(Phase::Closing);
        if !self.auth {
            self.outbox.push_drop_oldest(Work::Close);
            return;
        }
        log::info!("aa: saying goodbye");
        self.send_signal(
            CH_CONTROL,
            MSG_BYEBYE_REQUEST,
            &proto::ByeByeRequest { reason: 1 },
        );
        self.close_by = now + BYEBYE_WAIT_MS;
    }

    // --- Time ---

    /// The clock moved: pings, the timeouts, the pending focus toggle and
    /// the goodbye wait.
    pub fn on_tick(&mut self, now: u64, ts_ns: u64) {
        if self.phase == Phase::Closed {
            return;
        }
        if self.close_by != 0 && now >= self.close_by {
            log::info!("aa: no answer to the goodbye, closing");
            self.close_by = 0;
            self.outbox.push_drop_oldest(Work::Close);
            return;
        }
        if self.ending {
            return;
        }
        if self.toggle_at != 0 && now >= self.toggle_at {
            self.toggle_at = 0;
            self.notify_focus(true, true);
        }
        if !self.live_once && now >= self.started + SETUP_TIMEOUT_MS {
            log::warn!(
                "aa: no video {} s after the link came up",
                SETUP_TIMEOUT_MS / 1000
            );
            self.fail(Fault::SetupTimeout);
            return;
        }
        if !self.auth {
            return;
        }
        // Only a ping that went unanswered, by anything at all from the
        // phone, means it is really gone: a backgrounded phone is quiet
        // but answers.
        if self.unanswered_since != 0 && now >= self.unanswered_since + self.ping_timeout {
            log::warn!("aa: pings unanswered for {} ms", self.ping_timeout);
            self.fail(Fault::Silent);
            return;
        }
        if now >= self.next_ping {
            self.send(
                CH_CONTROL,
                Class::Plaintext,
                MSG_PING_REQUEST,
                &proto::PingRequest {
                    timestamp: ts_ns as i64,
                },
            );
            if self.unanswered_since == 0 {
                self.unanswered_since = now;
            }
            self.next_ping = now + self.ping_interval;
        }
    }

    /// When `on_tick` next has something to do, in milliseconds from `now`.
    pub fn next_due(&self, now: u64) -> u64 {
        let mut due = u64::MAX;
        let mut consider = |at: u64| {
            if at != 0 {
                due = due.min(at.saturating_sub(now));
            }
        };
        if self.phase == Phase::Closed {
            return due;
        }
        consider(self.close_by);
        if self.ending {
            return due;
        }
        consider(self.toggle_at);
        if !self.live_once {
            consider(self.started + SETUP_TIMEOUT_MS);
        }
        if self.auth {
            consider(self.next_ping);
            if self.unanswered_since != 0 {
                consider(self.unanswered_since + self.ping_timeout);
            }
        }
        due
    }
}

fn audio_kind(ch: u8) -> AudioKind {
    match ch {
        CH_MEDIA_AUDIO => AudioKind::Media,
        CH_SPEECH_AUDIO => AudioKind::Speech,
        _ => AudioKind::System,
    }
}

/// What a phone button sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Key(u32),
    Rotary(i32),
}

/// With a rotary controller declared, Android Auto takes the D-pad as its
/// nudge: a jump to the next area (the rail, the content, a card), never
/// to the next item; turning does that. So Up and Down turn, walking a
/// list as the wheel does, and Left and Right nudge between areas. Menu
/// is the loop's own (it brings the screens up); the knob's turns arrive
/// as `Cmd::Rotary` but are mapped here as well.
pub fn button_action(button: PhoneButton) -> Option<Action> {
    Some(match button {
        PhoneButton::Up => Action::Rotary(-1),
        PhoneButton::Down => Action::Rotary(1),
        PhoneButton::Left => Action::Key(KEY_DPAD_LEFT),
        PhoneButton::Right => Action::Key(KEY_DPAD_RIGHT),
        PhoneButton::Select => Action::Key(KEY_DPAD_CENTER),
        PhoneButton::Back => Action::Key(KEY_BACK),
        PhoneButton::Home => Action::Key(KEY_HOME),
        PhoneButton::PlayPause => Action::Key(KEY_PLAY_PAUSE),
        PhoneButton::Play => Action::Key(KEY_MEDIA_PLAY),
        PhoneButton::Pause => Action::Key(KEY_MEDIA_PAUSE),
        PhoneButton::NextTrack => Action::Key(KEY_MEDIA_NEXT),
        PhoneButton::PrevTrack => Action::Key(KEY_MEDIA_PREVIOUS),
        PhoneButton::Siri => Action::Key(KEY_SEARCH),
        PhoneButton::RotateNext => Action::Rotary(1),
        PhoneButton::RotatePrev => Action::Rotary(-1),
        PhoneButton::Menu => return None,
    })
}

/// One touch frame as the phone wants it: every contact scaled from the
/// panel's `TOUCH_SCALE` units to the video's pixels, the slot as the
/// pointer id, and one action for the whole report, which names the
/// contact that changed. Fixed point: a multiply and a divide per axis.
pub fn touch_report(
    ts_ns: u64,
    contacts: &[Contact; MAX_SLOTS],
    count: u8,
    width: u32,
    height: u32,
) -> Option<proto::InputReport> {
    use lehu_input::Phase as Touch;
    let n = usize::from(count).min(MAX_SLOTS);
    if n == 0 {
        return None;
    }
    let scale = |v: u16, px: u32| (u32::from(v.min(TOUCH_SCALE)) * px) / u32::from(TOUCH_SCALE);
    let mut pointers = Vec::with_capacity(n);
    let mut changed = None;
    for (i, c) in contacts[..n].iter().enumerate() {
        pointers.push(proto::Pointer {
            x: scale(c.x, width),
            y: scale(c.y, height),
            pointer_id: u32::from(c.slot),
        });
        if matches!(c.phase, Touch::Down | Touch::Up) {
            changed = Some((i, c.phase));
        }
    }
    let (index, action) = match changed {
        Some((i, Touch::Down)) => (
            i,
            if n == 1 {
                TOUCH_DOWN
            } else {
                TOUCH_POINTER_DOWN
            },
        ),
        Some((i, Touch::Up)) => (i, if n == 1 { TOUCH_UP } else { TOUCH_POINTER_UP }),
        _ => (0, TOUCH_MOVED),
    };
    Some(proto::InputReport {
        timestamp: ts_ns,
        touch_event: Some(proto::TouchEvent {
            pointer_data: pointers,
            action_index: Some(index as u32),
            action: Some(action),
        }),
        key_event: None,
        relative_event: None,
    })
}

/// Whether a touch command only moves (no contact went down or up): such
/// reports are collapsed when the write thread is behind.
pub fn touch_is_move_only(cmd: &Cmd) -> bool {
    match cmd {
        Cmd::Touch { count, contacts } => contacts[..usize::from(*count).min(MAX_SLOTS)]
            .iter()
            .all(|c| c.phase == lehu_input::Phase::Move),
        _ => false,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    use lehu_core::poll::Wake;
    use lehu_input::Phase as Touch;
    use lehu_session::{AudioFormat, Link};

    use crate::frame::{Fragments, parse_total};

    fn err(r: Result<Option<AccessUnit>, Fatal>) -> Fatal {
        match r {
            Ok(_) => panic!("not an error"),
            Err(e) => e,
        }
    }

    /// What the fake sink was given.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Sunk {
        Pcm(AudioKind, u32, u8, usize),
        Mic(Option<MicFormat>),
    }

    pub(crate) struct FakeSinks(pub Arc<Mutex<Vec<Sunk>>>);

    impl Sinks for FakeSinks {
        fn audio_pcm(&mut self, kind: AudioKind, rate: u32, channels: u8, pcm: &[i16]) {
            self.0
                .lock()
                .unwrap()
                .push(Sunk::Pcm(kind, rate, channels, pcm.len()));
        }
        fn audio_packet(&mut self, _: AudioKind, _: AudioFormat, _: &[u8]) {}
        fn mic(&mut self, wanted: Option<MicFormat>) {
            self.0.lock().unwrap().push(Sunk::Mic(wanted));
        }
    }

    pub(crate) fn config() -> Config {
        Config::from_settings(&Settings::default())
    }

    /// A message the machine queued, decrypted by the phone's end.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Sent {
        Msg(u8, Class, u16, Vec<u8>),
        Cmd(Cmd),
        Close,
    }

    /// The machine with its ports, the phone's TLS end, and the clock.
    pub(crate) struct Rig {
        pub m: Machine,
        pub phone: Tls,
        pub outbox: Arc<Queue<Work>>,
        pub events: Arc<Queue<Event>>,
        pub ports: Ports,
        pub sunk: Arc<Mutex<Vec<Sunk>>>,
        pub recycle: Arc<Queue<AccessUnit>>,
        pub now: u64,
        pub phase: Arc<AtomicU8>,
        pub name: Arc<OnceLock<String>>,
        pub identity: Arc<OnceLock<String>>,
        /// What the phone sees as the TLS session: decrypting what the
        /// machine queued, encrypting what it sends.
        cipher: Vec<u8>,
        frame: Vec<u8>,
    }

    impl Rig {
        pub(crate) fn new(config: Config) -> Self {
            let wake = Arc::new(Wake::new().unwrap());
            let video = Arc::new(Queue::new(8));
            let recycle = Arc::new(Queue::new(8));
            let ports = Ports::new(wake, video, Arc::clone(&recycle));
            let events = Arc::clone(&ports.events);
            let kept = ports.clone();
            let sunk = Arc::new(Mutex::new(Vec::new()));
            let outbox = Arc::new(Queue::new(OUTBOX_DEPTH));
            let phase = Arc::new(AtomicU8::new(0));
            let name = Arc::new(OnceLock::new());
            let identity = Arc::new(OnceLock::new());
            let force = config.force_chacha20;
            let mut m = Machine::new(
                config,
                Tls::new(force).unwrap(),
                Arc::clone(&outbox),
                ports,
                Box::new(FakeSinks(Arc::clone(&sunk))),
                Arc::clone(&phase),
                Arc::clone(&name),
                Arc::clone(&identity),
                1000,
            );
            m.start();
            Self {
                m,
                phone: crate::tls::test_phone(None),
                outbox,
                events,
                ports: kept,
                sunk,
                recycle,
                now: 1000,
                phase,
                name,
                identity,
                cipher: Vec::new(),
                frame: Vec::new(),
            }
        }

        /// Everything queued since the last call, as the phone reads it.
        pub(crate) fn sent(&mut self) -> Vec<Sent> {
            let mut out = Vec::new();
            while let Some(w) = self.outbox.pop() {
                out.push(match w {
                    Work::Close => Sent::Close,
                    Work::Cmd(c) => Sent::Cmd(c),
                    Work::Msg(m) => {
                        let body = if m.class.encrypted() {
                            let mut plain = Vec::new();
                            let mut records = Vec::new();
                            let mut head = m.msg_id.to_be_bytes().to_vec();
                            head.extend_from_slice(m.body());
                            self.m.encrypt(&head, &mut records).unwrap();
                            self.phone.decrypt(&records, &mut plain).unwrap();
                            assert_eq!(&plain[..2], &m.msg_id.to_be_bytes());
                            plain[2..].to_vec()
                        } else {
                            m.body().to_vec()
                        };
                        Sent::Msg(m.channel, m.class, m.msg_id, body)
                    }
                });
            }
            out
        }

        pub(crate) fn events(&self) -> Vec<Event> {
            let mut out = Vec::new();
            while let Some(e) = self.events.pop() {
                out.push(e);
            }
            out
        }

        /// The phone sends a message: framed, encrypted when the class
        /// says so, fed to the machine fragment by fragment.
        pub(crate) fn phone_sends(
            &mut self,
            ch: u8,
            class: Class,
            msg_id: u16,
            body: &[u8],
        ) -> Result<Option<AccessUnit>, Fatal> {
            let mut plain = msg_id.to_be_bytes().to_vec();
            plain.extend_from_slice(body);
            self.cipher.clear();
            if class.encrypted() {
                self.phone.encrypt(&plain, &mut self.cipher).unwrap();
            } else {
                self.cipher.extend_from_slice(&plain);
            }
            let mut fragments = Fragments::new(ch, class, &self.cipher);
            let mut unit = None;
            while fragments.next_into(&mut self.frame) {
                let header = Header::parse(self.frame[..4].try_into().unwrap()).unwrap();
                let (total, start) = if header.has_total() {
                    (
                        parse_total(self.frame[4..8].try_into().unwrap()).unwrap(),
                        8,
                    )
                } else {
                    (0, 4)
                };
                let payload = self.frame[start..].to_vec();
                if let Some(u) = self.m.on_frame(self.now, &header, total, &payload)? {
                    unit = Some(u);
                }
            }
            Ok(unit)
        }

        pub(crate) fn phone_sends_proto(
            &mut self,
            ch: u8,
            msg_id: u16,
            m: &impl prost::Message,
        ) -> Result<Option<AccessUnit>, Fatal> {
            let mut buf = [0u8; OUT_BODY];
            let n = proto::encode_into(m, &mut buf).unwrap();
            self.phone_sends(ch, Class::EncSignal, msg_id, &buf[..n])
        }

        /// The version handshake and the TLS handshake, to auth complete.
        pub(crate) fn connect(&mut self) {
            let sent = self.sent();
            assert_eq!(
                sent,
                [Sent::Msg(
                    CH_CONTROL,
                    Class::Plaintext,
                    MSG_VERSION_REQUEST,
                    vec![0, 1, 0, 7]
                )]
            );
            self.phone_sends(
                CH_CONTROL,
                Class::Plaintext,
                MSG_VERSION_RESPONSE,
                &[0, 1, 0, 7, 0, 0],
            )
            .unwrap();
            let mut to_phone = Vec::new();
            let mut rounds = 0;
            loop {
                rounds += 1;
                assert!(rounds < 8, "the handshake does not converge");
                let mut done = false;
                for s in self.sent() {
                    match s {
                        Sent::Msg(CH_CONTROL, Class::Plaintext, MSG_SSL_HANDSHAKE, body) => {
                            to_phone.extend_from_slice(&body);
                        }
                        Sent::Msg(CH_CONTROL, Class::Plaintext, MSG_AUTH_COMPLETE, body) => {
                            assert_eq!(body, [0x08, 0x00]);
                            done = true;
                        }
                        other => panic!("unexpected {other:?} during the handshake"),
                    }
                }
                if done {
                    break;
                }
                let mut reply = Vec::new();
                self.phone.accept_step(&to_phone, &mut reply).unwrap();
                to_phone.clear();
                if !reply.is_empty() {
                    self.phone_sends(CH_CONTROL, Class::Plaintext, MSG_SSL_HANDSHAKE, &reply)
                        .unwrap();
                }
            }
            assert!(self.phone.ready());
        }

        /// Discovery and the channel opens the phone does first.
        pub(crate) fn discover(&mut self) {
            self.phone_sends_proto(
                CH_CONTROL,
                MSG_SERVICE_DISCOVERY_REQUEST,
                &proto::ServiceDiscoveryRequest {
                    device_name: Some("Pixel 8".into()),
                    phone_info: Some(proto::PhoneInfo {
                        instance_id: Some("abc123".into()),
                    }),
                },
            )
            .unwrap();
            for ch in [
                CH_SENSOR,
                CH_VIDEO,
                CH_MEDIA_AUDIO,
                CH_SPEECH_AUDIO,
                CH_SYSTEM_AUDIO,
                CH_INPUT,
                CH_MIC,
            ] {
                self.phone_sends_proto(
                    ch,
                    MSG_CHANNEL_OPEN_REQUEST,
                    &proto::ChannelOpenRequest {
                        priority: 0,
                        service_id: i32::from(ch),
                    },
                )
                .unwrap();
            }
        }

        /// Up to a live video stream.
        pub(crate) fn go_live(&mut self) {
            self.connect();
            self.discover();
            self.phone_sends_proto(
                CH_VIDEO,
                MSG_MEDIA_SETUP,
                &proto::MediaSetup { codec_type: 3 },
            )
            .unwrap();
            self.phone_sends_proto(
                CH_VIDEO,
                MSG_MEDIA_START,
                &proto::MediaStart {
                    session_id: 5,
                    configuration_index: 0,
                },
            )
            .unwrap();
        }

        pub(crate) fn tick(&mut self, at: u64) {
            self.now = at;
            self.m.on_tick(at, at * 1_000_000);
        }
    }

    fn ids(sent: &[Sent]) -> Vec<(u8, u16)> {
        sent.iter()
            .filter_map(|s| match s {
                Sent::Msg(ch, _, id, _) => Some((*ch, *id)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_version_mismatch_is_a_rejection() {
        let mut r = Rig::new(config());
        r.sent();
        let e = err(r.phone_sends(
            CH_CONTROL,
            Class::Plaintext,
            MSG_VERSION_RESPONSE,
            &[0, 1, 0, 7, 0, 0xff],
        ));
        assert!(matches!(e, Fatal::Rejected(_)));
        assert_eq!(e.fault(), Fault::Rejected);
        let e = err(r.phone_sends(CH_CONTROL, Class::Plaintext, MSG_VERSION_RESPONSE, &[0, 1]));
        assert!(matches!(e, Fatal::Rejected(_)));
    }

    #[test]
    fn the_handshake_leads_to_discovery_and_the_channels() {
        let mut r = Rig::new(config());
        r.connect();
        assert!(r.events().is_empty());
        r.discover();
        let sent = r.sent();
        assert_eq!(ids(&sent)[0], (CH_CONTROL, MSG_SERVICE_DISCOVERY_RESPONSE));
        let Sent::Msg(_, class, _, body) = &sent[0] else {
            panic!()
        };
        assert_eq!(*class, Class::EncSignal);
        let sdr: proto::ServiceDiscoveryResponse = proto::decode(body).unwrap();
        assert_eq!(sdr.channels.len(), 9);
        // Seven channel open responses, control class, and the driving
        // status right behind the sensor channel's.
        let opens: Vec<&Sent> = sent
            .iter()
            .filter(|s| {
                matches!(
                    s,
                    Sent::Msg(_, Class::EncControl, MSG_CHANNEL_OPEN_RESPONSE, _)
                )
            })
            .collect();
        assert_eq!(opens.len(), 7);
        assert_eq!(
            sent[2],
            Sent::Msg(
                CH_SENSOR,
                Class::EncSignal,
                MSG_SENSOR_BATCH,
                vec![0x6a, 0x02, 0x08, 0x00]
            )
        );
        assert_eq!(r.events(), [Event::Phase(Phase::Identified)]);
        assert_eq!(r.name.get().map(String::as_str), Some("Pixel 8"));
        assert_eq!(r.identity.get().map(String::as_str), Some("abc123"));
        assert_eq!(r.phase.load(Ordering::Relaxed), Phase::Identified as u8);
    }

    #[test]
    fn now_playing_and_guidance_reach_the_side_channel() {
        let mut r = Rig::new(config());
        r.connect();
        r.discover();
        for ch in [CH_NAVIGATION, CH_MEDIA_INFO] {
            r.phone_sends_proto(
                ch,
                MSG_CHANNEL_OPEN_REQUEST,
                &proto::ChannelOpenRequest {
                    priority: 0,
                    service_id: i32::from(ch),
                },
            )
            .unwrap();
        }
        let opened = r.sent();
        assert_eq!(
            ids(&opened)[ids(&opened).len() - 2..],
            [
                (CH_NAVIGATION, MSG_CHANNEL_OPEN_RESPONSE),
                (CH_MEDIA_INFO, MSG_CHANNEL_OPEN_RESPONSE)
            ]
        );
        r.events();
        r.phone_sends_proto(
            CH_MEDIA_INFO,
            crate::status::MSG_PLAYBACK_METADATA,
            &proto::MediaPlaybackMetadata {
                title: Some("Song".into()),
                ..Default::default()
            },
        )
        .unwrap();
        r.phone_sends_proto(
            CH_NAVIGATION,
            crate::status::MSG_NAV_NEXT_TURN_DISTANCE,
            &proto::NavigationNextTurnDistance {
                distance_m: 80,
                time_to_turn_seconds: 5,
            },
        )
        .unwrap();
        // Garbage changes nothing and names nothing.
        r.phone_sends(
            CH_NAVIGATION,
            Class::EncSignal,
            crate::status::MSG_NAV_NEXT_TURN,
            &[0x0a, 0x7f],
        )
        .unwrap();
        assert_eq!(r.events(), [Event::NowPlaying, Event::Guidance]);
        let mut np = lehu_session::NowPlaying::default();
        r.ports.now_playing.read_into(0, &mut np).unwrap();
        assert_eq!(np.title, "Song");
        let mut g = lehu_session::Guidance::default();
        r.ports.guidance.read_into(0, &mut g).unwrap();
        assert_eq!((g.active, g.distance_m), (true, Some(80)));
        // Nothing goes back to the phone on either channel.
        assert!(
            ids(&r.sent())
                .iter()
                .all(|(ch, _)| *ch != CH_NAVIGATION && *ch != CH_MEDIA_INFO)
        );
    }

    #[test]
    fn a_setup_takes_the_configuration_of_its_codec() {
        let mut config = config();
        config.offer.codecs = [Some(Codec::Hevc), Some(Codec::H264)];
        for (codec_type, index, codec) in [
            (CODEC_VIDEO_H265, 0, Codec::Hevc),
            (CODEC_VIDEO_H264_BP, 1, Codec::H264),
            // Not offered: the first configuration, decoded as H.264.
            (CODEC_VIDEO_VP9, 0, Codec::H264),
        ] {
            let mut r = Rig::new(config.clone());
            r.connect();
            r.discover();
            r.phone_sends_proto(CH_VIDEO, MSG_MEDIA_SETUP, &proto::MediaSetup { codec_type })
                .unwrap();
            let sent = r.sent();
            let body = sent
                .iter()
                .find_map(|s| match s {
                    Sent::Msg(CH_VIDEO, _, MSG_MEDIA_CONFIG, b) => Some(b.clone()),
                    _ => None,
                })
                .unwrap();
            let cfg: proto::MediaConfig = proto::decode(&body).unwrap();
            assert_eq!(cfg.configuration_indices, [index], "codec {codec_type}");
            assert!(r.events().contains(&Event::VideoCodec(codec)));
        }
    }

    #[test]
    fn video_setup_start_data_and_stop() {
        let mut r = Rig::new(config());
        r.go_live();
        let sent = r.sent();
        let got = ids(&sent);
        let setup = got
            .iter()
            .position(|&x| x == (CH_VIDEO, MSG_MEDIA_CONFIG))
            .unwrap();
        assert_eq!(got[setup + 1], (CH_VIDEO, MSG_VIDEO_FOCUS_NOTIFICATION));
        let Sent::Msg(_, _, _, config_body) = &sent[setup] else {
            panic!()
        };
        let cfg: proto::MediaConfig = proto::decode(config_body).unwrap();
        assert_eq!(cfg.max_unacked, Some(config().offer.max_unacked));
        assert_eq!(cfg.configuration_indices, [0]);
        let Sent::Msg(_, _, _, focus) = &sent[setup + 1] else {
            panic!()
        };
        let f: proto::VideoFocusNotification = proto::decode(focus).unwrap();
        assert_eq!(
            (f.focus, f.unsolicited),
            (Some(VIDEO_FOCUS_PROJECTED), Some(true))
        );
        let events = r.events();
        assert_eq!(
            &events[events.len() - 3..],
            &[
                Event::VideoCodec(Codec::H264),
                Event::Phase(Phase::Ready),
                Event::Phase(Phase::Live)
            ]
        );
        // A frame: timestamp then the bytes; acknowledged with the session.
        let mut data = vec![0u8; 8];
        data.extend_from_slice(&[0, 0, 0, 1, 0x65, 1, 2, 3]);
        let unit = r
            .phone_sends(CH_VIDEO, Class::EncSignal, MSG_MEDIA_DATA, &data)
            .unwrap()
            .unwrap();
        assert_eq!(unit.data(), &[0, 0, 0, 1, 0x65, 1, 2, 3]);
        assert!(!unit.config);
        assert_eq!(unit.capacity(), UNIT_BYTES);
        assert_eq!(
            r.sent(),
            [Sent::Msg(
                CH_VIDEO,
                Class::EncSignal,
                MSG_MEDIA_ACK,
                vec![0x08, 0x05, 0x10, 0x01]
            )]
        );
        // Parameter sets come without a timestamp and are flagged.
        let unit = r
            .phone_sends(
                CH_VIDEO,
                Class::EncSignal,
                MSG_MEDIA_CODEC_CONFIG,
                &[0, 0, 0, 1, 0x67, 9],
            )
            .unwrap()
            .unwrap();
        assert!(unit.config);
        assert_eq!(unit.len(), 6);
        // A recycled unit is reused when it is big enough.
        r.recycle.push_discard(AccessUnit::with_capacity(2));
        r.recycle.push_discard(AccessUnit::with_capacity(64));
        let unit = r
            .phone_sends(CH_VIDEO, Class::EncSignal, MSG_MEDIA_DATA, &data)
            .unwrap()
            .unwrap();
        assert_eq!(unit.capacity(), UNIT_BYTES, "the small one is dropped");
        let unit = r
            .phone_sends(CH_VIDEO, Class::EncSignal, MSG_MEDIA_DATA, &data)
            .unwrap()
            .unwrap();
        assert_eq!(unit.capacity(), 64);
        // Too short for a timestamp: nothing, no ack.
        assert!(
            r.phone_sends(CH_VIDEO, Class::EncSignal, MSG_MEDIA_DATA, &[1, 2])
                .unwrap()
                .is_none()
        );
        r.sent();
        r.phone_sends(CH_VIDEO, Class::EncSignal, MSG_MEDIA_STOP, &[])
            .unwrap();
        assert_eq!(r.events().last(), Some(&Event::Phase(Phase::Backgrounded)));
        r.phone_sends_proto(
            CH_VIDEO,
            MSG_MEDIA_START,
            &proto::MediaStart {
                session_id: 6,
                configuration_index: 0,
            },
        )
        .unwrap();
        assert_eq!(r.events().last(), Some(&Event::Phase(Phase::Live)));
        // A second setup does not report Ready again.
        r.phone_sends_proto(
            CH_VIDEO,
            MSG_MEDIA_SETUP,
            &proto::MediaSetup { codec_type: 3 },
        )
        .unwrap();
        assert!(r.events().is_empty());
    }

    #[test]
    fn audio_goes_to_the_sink_as_samples_and_the_mic_is_served() {
        let mut r = Rig::new(config());
        r.go_live();
        r.sent();
        r.events();
        r.phone_sends_proto(
            CH_MEDIA_AUDIO,
            MSG_MEDIA_SETUP,
            &proto::MediaSetup { codec_type: 1 },
        )
        .unwrap();
        assert_eq!(ids(&r.sent()), [(CH_MEDIA_AUDIO, MSG_MEDIA_CONFIG)]);
        r.phone_sends_proto(
            CH_MEDIA_AUDIO,
            MSG_MEDIA_START,
            &proto::MediaStart {
                session_id: 2,
                configuration_index: 0,
            },
        )
        .unwrap();
        assert_eq!(r.events(), [Event::Audio(AudioKind::Media, true)]);
        let mut pcm = vec![0u8; 8];
        pcm.extend_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0, 5]);
        r.phone_sends(CH_MEDIA_AUDIO, Class::EncSignal, MSG_MEDIA_DATA, &pcm)
            .unwrap();
        assert_eq!(
            r.sunk.lock().unwrap().as_slice(),
            [Sunk::Pcm(AudioKind::Media, 48000, 2, 4)]
        );
        assert_eq!(
            r.sent(),
            [Sent::Msg(
                CH_MEDIA_AUDIO,
                Class::EncSignal,
                MSG_MEDIA_ACK,
                vec![0x08, 0x02, 0x10, 0x01]
            )]
        );
        r.sunk.lock().unwrap().clear();
        r.phone_sends(CH_SPEECH_AUDIO, Class::EncSignal, MSG_MEDIA_DATA, &pcm)
            .unwrap();
        r.phone_sends(CH_SYSTEM_AUDIO, Class::EncSignal, MSG_MEDIA_DATA, &pcm)
            .unwrap();
        assert_eq!(
            r.sunk.lock().unwrap().as_slice(),
            [
                Sunk::Pcm(AudioKind::Speech, 16000, 1, 4),
                Sunk::Pcm(AudioKind::System, 16000, 1, 4)
            ]
        );
        r.phone_sends(CH_MEDIA_AUDIO, Class::EncSignal, MSG_MEDIA_STOP, &[])
            .unwrap();
        assert_eq!(r.events(), [Event::Audio(AudioKind::Media, false)]);
        r.sent();
        r.sunk.lock().unwrap().clear();
        // The microphone: setup, the open request, our start.
        r.phone_sends_proto(
            CH_MIC,
            MSG_MEDIA_SETUP,
            &proto::MediaSetup { codec_type: 1 },
        )
        .unwrap();
        r.phone_sends_proto(
            CH_MIC,
            MSG_MICROPHONE_REQUEST,
            &proto::MicrophoneRequest { open: true },
        )
        .unwrap();
        assert_eq!(
            ids(&r.sent()),
            [
                (CH_MIC, MSG_MEDIA_CONFIG),
                (CH_MIC, MSG_MICROPHONE_RESPONSE),
                (CH_MIC, MSG_MEDIA_START)
            ]
        );
        assert!(r.m.mic_open());
        let wanted = MicFormat {
            codec: AudioCodec::Pcm,
            rate: 16000,
            frame_tenths_ms: 200,
        };
        assert_eq!(r.sunk.lock().unwrap().as_slice(), [Sunk::Mic(Some(wanted))]);
        r.phone_sends_proto(
            CH_MIC,
            MSG_MICROPHONE_REQUEST,
            &proto::MicrophoneRequest { open: true },
        )
        .unwrap();
        assert_eq!(
            ids(&r.sent()),
            [(CH_MIC, MSG_MICROPHONE_RESPONSE)],
            "no second start"
        );
        r.phone_sends(CH_MIC, Class::EncSignal, MSG_MEDIA_ACK, &[0x08, 0x01])
            .unwrap();
        r.phone_sends(CH_MIC, Class::EncSignal, MSG_MEDIA_STOP, &[])
            .unwrap();
        assert!(!r.m.mic_open());
        assert_eq!(r.sunk.lock().unwrap().last(), Some(&Sunk::Mic(None)));
    }

    #[test]
    fn pings_go_at_the_interval_and_silence_ends_the_session() {
        let mut r = Rig::new(config());
        r.connect();
        r.sent();
        assert_eq!(r.m.next_due(1000), PING_INTERVAL_MS);
        r.tick(1000 + PING_INTERVAL_MS - 1);
        assert!(r.sent().is_empty());
        r.tick(1000 + PING_INTERVAL_MS);
        let sent = r.sent();
        assert_eq!(ids(&sent), [(CH_CONTROL, MSG_PING_REQUEST)]);
        assert!(
            matches!(&sent[0], Sent::Msg(_, Class::Plaintext, _, _)),
            "pings stay plaintext"
        );
        // Anything from the phone counts as life, not only a pong.
        r.now = 1000 + PING_INTERVAL_MS + 100;
        r.phone_sends_proto(
            CH_CONTROL,
            MSG_PING_REQUEST,
            &proto::PingRequest { timestamp: 77 },
        )
        .unwrap();
        assert_eq!(ids(&r.sent()), [(CH_CONTROL, MSG_PING_RESPONSE)]);
        r.tick(1000 + 2 * PING_INTERVAL_MS);
        assert_eq!(ids(&r.sent()), [(CH_CONTROL, MSG_PING_REQUEST)]);
        // Unanswered for the timeout: a fault and the close.
        let quiet = 1000 + 2 * PING_INTERVAL_MS + PING_TIMEOUT_MS - 1;
        r.tick(quiet);
        assert!(r.events().is_empty());
        assert_eq!(
            ids(&r.sent()),
            [(CH_CONTROL, MSG_PING_REQUEST)],
            "pings keep going"
        );
        r.tick(quiet + 1);
        assert_eq!(r.events(), [Event::Fault(Fault::Silent)]);
        assert_eq!(r.sent(), [Sent::Close]);
        assert!(r.m.ending());
        assert_eq!(r.m.next_due(quiet + 1), u64::MAX);
        r.m.finish(Some(Fault::LinkLost));
        assert_eq!(r.events(), [Event::Phase(Phase::Closed)], "no second fault");
        r.m.finish(None);
        assert!(r.events().is_empty());
    }

    #[test]
    fn the_phone_names_its_ping_interval() {
        let mut r = Rig::new(config());
        r.sent();
        // ConnectionConfiguration { PingConfiguration { timeout 2000, interval 500 } }
        let mut body = vec![0, 1, 0, 7, 0, 0];
        let ping = [0x08, 0xd0, 0x0f, 0x10, 0xf4, 0x03];
        body.extend_from_slice(&[0x0a, 8, 0x0a, 6]);
        body.extend_from_slice(&ping);
        r.phone_sends(CH_CONTROL, Class::Plaintext, MSG_VERSION_RESPONSE, &body)
            .unwrap();
        assert_eq!(r.m.ping_interval, 500);
        assert_eq!(r.m.ping_timeout, 2000);
    }

    #[test]
    fn no_video_within_thirty_seconds_is_a_setup_timeout() {
        let mut r = Rig::new(config());
        r.connect();
        r.discover();
        r.sent();
        r.events();
        assert_eq!(r.m.next_due(2000), PING_INTERVAL_MS - 1000);
        r.tick(1000 + SETUP_TIMEOUT_MS - 1);
        r.sent();
        assert!(!r.m.ending());
        r.tick(1000 + SETUP_TIMEOUT_MS);
        assert_eq!(r.events(), [Event::Fault(Fault::SetupTimeout)]);
        assert_eq!(r.sent().last(), Some(&Sent::Close));
        // A live session is never timed out.
        let mut r = Rig::new(config());
        r.go_live();
        r.sent();
        r.events();
        r.tick(1000 + SETUP_TIMEOUT_MS + 5);
        assert!(r.events().is_empty());
    }

    #[test]
    fn goodbye_waits_for_the_answer_or_a_second() {
        let mut r = Rig::new(config());
        r.go_live();
        r.sent();
        r.events();
        r.m.on_cmd(5000, 0, Cmd::Close);
        assert_eq!(r.events(), [Event::Phase(Phase::Closing)]);
        let sent = r.sent();
        assert_eq!(ids(&sent), [(CH_CONTROL, MSG_BYEBYE_REQUEST)]);
        assert_eq!(r.m.next_due(5000), BYEBYE_WAIT_MS);
        r.m.on_cmd(5000, 0, Cmd::Close);
        assert!(r.sent().is_empty(), "a second close is nothing");
        r.now = 5200;
        r.phone_sends(CH_CONTROL, Class::EncSignal, MSG_BYEBYE_RESPONSE, &[])
            .unwrap();
        assert_eq!(r.sent(), [Sent::Close]);
        r.tick(7000);
        assert!(r.sent().is_empty());
        r.m.finish(None);
        assert_eq!(r.events(), [Event::Phase(Phase::Closed)]);
        // Unanswered: the second passes.
        let mut r = Rig::new(config());
        r.go_live();
        r.sent();
        r.events();
        r.m.on_cmd(5000, 0, Cmd::Close);
        r.sent();
        r.tick(5000 + BYEBYE_WAIT_MS - 1);
        assert!(r.sent().is_empty());
        r.tick(5000 + BYEBYE_WAIT_MS);
        assert_eq!(r.sent(), [Sent::Close]);
        // Before TLS there is nothing to say.
        let mut r = Rig::new(config());
        r.sent();
        r.m.on_cmd(1000, 0, Cmd::Close);
        assert_eq!(r.sent(), [Sent::Close]);
        assert_eq!(r.events(), [Event::Phase(Phase::Closing)]);
    }

    #[test]
    fn the_phones_goodbye_is_answered_and_reported() {
        let mut r = Rig::new(config());
        r.go_live();
        r.sent();
        r.events();
        r.phone_sends_proto(
            CH_CONTROL,
            MSG_BYEBYE_REQUEST,
            &proto::ByeByeRequest { reason: 2 },
        )
        .unwrap();
        let sent = r.sent();
        assert_eq!(sent.len(), 2);
        assert!(matches!(
            sent[0],
            Sent::Msg(CH_CONTROL, Class::EncSignal, MSG_BYEBYE_RESPONSE, _)
        ));
        assert_eq!(sent[1], Sent::Close);
        assert_eq!(
            r.events(),
            [Event::Fault(Fault::PhoneLeft), Event::Phase(Phase::Closing)]
        );
        r.m.finish(Some(Fault::LinkLost));
        assert_eq!(r.events(), [Event::Phase(Phase::Closed)]);
    }

    fn focus_of(sent: &[Sent]) -> Vec<(i32, bool)> {
        sent.iter()
            .filter_map(|s| match s {
                Sent::Msg(CH_VIDEO, _, MSG_VIDEO_FOCUS_NOTIFICATION, body) => {
                    let f: proto::VideoFocusNotification = proto::decode(body).unwrap();
                    Some((f.focus.unwrap(), f.unsolicited.unwrap()))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_exit_button_is_honoured_and_resume_brings_the_video_back() {
        let mut r = Rig::new(config());
        r.go_live();
        r.sent();
        r.events();
        r.phone_sends_proto(
            CH_VIDEO,
            MSG_VIDEO_FOCUS_REQUEST,
            &proto::VideoFocusRequestNotification {
                mode: Some(VIDEO_FOCUS_NATIVE),
                reason: Some(2),
            },
        )
        .unwrap();
        assert_eq!(focus_of(&r.sent()), [(VIDEO_FOCUS_NATIVE, false)]);
        assert_eq!(r.events(), [Event::HostUi]);
        // The loop's own "the screens are up" says nothing new.
        r.m.on_cmd(2000, 0, Cmd::VideoActive(false));
        assert!(r.sent().is_empty());
        r.m.on_cmd(2000, 0, Cmd::Resume);
        assert_eq!(focus_of(&r.sent()), [(VIDEO_FOCUS_PROJECTED, true)]);
        r.m.on_cmd(2000, 0, Cmd::VideoActive(true));
        assert!(r.sent().is_empty());
        // The camera takes the screen and gives it back.
        r.m.on_cmd(2000, 0, Cmd::VideoActive(false));
        r.m.on_cmd(2000, 0, Cmd::VideoActive(true));
        assert_eq!(
            focus_of(&r.sent()),
            [(VIDEO_FOCUS_NATIVE, true), (VIDEO_FOCUS_PROJECTED, true)]
        );
        // The phone asks for the screen (a transient over): projected.
        r.phone_sends_proto(
            CH_VIDEO,
            MSG_VIDEO_FOCUS_REQUEST,
            &proto::VideoFocusRequestNotification {
                mode: Some(VIDEO_FOCUS_PROJECTED),
                reason: None,
            },
        )
        .unwrap();
        assert_eq!(focus_of(&r.sent()), [(VIDEO_FOCUS_PROJECTED, false)]);
        assert_eq!(r.events(), [Event::VideoFocus(true)]);
        // An unset mode is "show me".
        r.phone_sends(CH_VIDEO, Class::EncSignal, MSG_VIDEO_FOCUS_REQUEST, &[])
            .unwrap();
        assert_eq!(focus_of(&r.sent()), [(VIDEO_FOCUS_PROJECTED, false)]);
    }

    #[test]
    fn the_other_answer_to_the_exit_button_is_one_flag_away() {
        let mut r = Rig::new(Config {
            native_answer: NativeAnswer::Projected,
            ..config()
        });
        r.go_live();
        r.sent();
        r.events();
        r.phone_sends_proto(
            CH_VIDEO,
            MSG_VIDEO_FOCUS_REQUEST,
            &proto::VideoFocusRequestNotification {
                mode: Some(VIDEO_FOCUS_NATIVE_TRANSIENT),
                reason: None,
            },
        )
        .unwrap();
        assert_eq!(focus_of(&r.sent()), [(VIDEO_FOCUS_PROJECTED, false)]);
        assert_eq!(r.events(), [Event::HostUi]);
        // The loop then takes the screen for real.
        r.m.on_cmd(2000, 0, Cmd::VideoActive(false));
        assert_eq!(focus_of(&r.sent()), [(VIDEO_FOCUS_NATIVE, true)]);
    }

    #[test]
    fn a_keyframe_is_a_stream_restart_only_when_asked_for() {
        let mut r = Rig::new(config());
        r.go_live();
        r.sent();
        r.m.on_cmd(2000, 0, Cmd::Keyframe);
        assert!(r.sent().is_empty());
        let mut r = Rig::new(Config {
            keyframe_by_focus: true,
            ..config()
        });
        r.go_live();
        r.sent();
        r.m.on_cmd(2000, 0, Cmd::Keyframe);
        assert_eq!(focus_of(&r.sent()), [(VIDEO_FOCUS_NATIVE, true)]);
        assert_eq!(r.m.next_due(2000), KEYFRAME_TOGGLE_MS);
        r.tick(2000 + KEYFRAME_TOGGLE_MS);
        assert_eq!(focus_of(&r.sent()), [(VIDEO_FOCUS_PROJECTED, true)]);
        r.m.on_cmd(2500, 0, Cmd::Keyframe);
        assert!(r.sent().is_empty(), "rate limited");
        r.m.on_cmd(2000 + KEYFRAME_TOGGLE_MIN_GAP_MS, 0, Cmd::Keyframe);
        assert_eq!(focus_of(&r.sent()).len(), 1);
    }

    #[test]
    fn touch_is_scaled_to_the_phones_pixels() {
        let mut contacts = [Contact::default(); MAX_SLOTS];
        contacts[0] = Contact {
            slot: 0,
            x: TOUCH_SCALE,
            y: 5000,
            phase: Touch::Down,
        };
        let r = touch_report(9, &contacts, 1, 800, 480).unwrap();
        let t = r.touch_event.unwrap();
        assert_eq!(r.timestamp, 9);
        assert_eq!(
            (
                t.pointer_data[0].x,
                t.pointer_data[0].y,
                t.pointer_data[0].pointer_id
            ),
            (800, 240, 0)
        );
        assert_eq!((t.action, t.action_index), (Some(TOUCH_DOWN), Some(0)));
        contacts[0].phase = Touch::Move;
        contacts[0].x = 2500;
        let t = touch_report(9, &contacts, 1, 1920, 1080)
            .unwrap()
            .touch_event
            .unwrap();
        assert_eq!((t.pointer_data[0].x, t.pointer_data[0].y), (480, 540));
        assert_eq!(t.action, Some(TOUCH_MOVED));
        // A second finger: pointer down names it; its lift is pointer up.
        contacts[1] = Contact {
            slot: 3,
            x: 0,
            y: 0,
            phase: Touch::Down,
        };
        let t = touch_report(9, &contacts, 2, 800, 480)
            .unwrap()
            .touch_event
            .unwrap();
        assert_eq!(
            (t.action, t.action_index),
            (Some(TOUCH_POINTER_DOWN), Some(1))
        );
        assert_eq!(t.pointer_data[1].pointer_id, 3);
        contacts[1].phase = Touch::Up;
        let t = touch_report(9, &contacts, 2, 800, 480)
            .unwrap()
            .touch_event
            .unwrap();
        assert_eq!(
            (t.action, t.action_index),
            (Some(TOUCH_POINTER_UP), Some(1))
        );
        contacts[0].phase = Touch::Up;
        let t = touch_report(9, &contacts, 1, 800, 480)
            .unwrap()
            .touch_event
            .unwrap();
        assert_eq!(t.action, Some(TOUCH_UP));
        // Out of range is clamped, not wrapped; no contacts is no report.
        contacts[0] = Contact {
            slot: 0,
            x: u16::MAX,
            y: u16::MAX,
            phase: Touch::Move,
        };
        let t = touch_report(9, &contacts, 1, 800, 480)
            .unwrap()
            .touch_event
            .unwrap();
        assert_eq!((t.pointer_data[0].x, t.pointer_data[0].y), (800, 480));
        assert!(touch_report(9, &contacts, 0, 800, 480).is_none());
        assert!(touch_is_move_only(&Cmd::Touch { count: 1, contacts }));
        contacts[0].phase = Touch::Up;
        assert!(!touch_is_move_only(&Cmd::Touch { count: 1, contacts }));
        assert!(!touch_is_move_only(&Cmd::Rotary(1)));
    }

    #[test]
    fn keys_and_the_rotary_translate() {
        assert_eq!(button_action(PhoneButton::Up), Some(Action::Rotary(-1)));
        assert_eq!(button_action(PhoneButton::Down), Some(Action::Rotary(1)));
        assert_eq!(
            button_action(PhoneButton::Left),
            Some(Action::Key(KEY_DPAD_LEFT))
        );
        assert_eq!(
            button_action(PhoneButton::Select),
            Some(Action::Key(KEY_DPAD_CENTER))
        );
        assert_eq!(
            button_action(PhoneButton::Siri),
            Some(Action::Key(KEY_SEARCH))
        );
        assert_eq!(
            button_action(PhoneButton::PlayPause),
            Some(Action::Key(KEY_PLAY_PAUSE))
        );
        assert_eq!(
            button_action(PhoneButton::RotatePrev),
            Some(Action::Rotary(-1))
        );
        assert_eq!(button_action(PhoneButton::Menu), None);
        let mut r = Rig::new(config());
        r.connect();
        r.sent();
        // Before the input channel is open, keys are dropped.
        r.m.on_cmd(
            2000,
            7,
            Cmd::Key {
                button: PhoneButton::Select,
                down: true,
            },
        );
        assert!(r.sent().is_empty());
        r.discover();
        r.sent();
        r.m.on_cmd(
            2000,
            7,
            Cmd::Key {
                button: PhoneButton::Select,
                down: true,
            },
        );
        r.m.on_cmd(
            2000,
            7,
            Cmd::Key {
                button: PhoneButton::Select,
                down: false,
            },
        );
        let sent = r.sent();
        assert_eq!(
            ids(&sent),
            [(CH_INPUT, MSG_INPUT_REPORT), (CH_INPUT, MSG_INPUT_REPORT)]
        );
        let keys: Vec<(u32, bool)> = sent
            .iter()
            .map(|s| {
                let Sent::Msg(_, _, _, body) = s else {
                    panic!()
                };
                let r: proto::InputReport = proto::decode(body).unwrap();
                assert_eq!(r.timestamp, 7);
                let k = &r.key_event.unwrap().keys[0];
                (k.keycode, k.down)
            })
            .collect();
        assert_eq!(keys, [(KEY_DPAD_CENTER, true), (KEY_DPAD_CENTER, false)]);
        r.m.on_cmd(
            2000,
            7,
            Cmd::Key {
                button: PhoneButton::Up,
                down: true,
            },
        );
        r.m.on_cmd(2000, 7, Cmd::Rotary(3));
        r.m.on_cmd(2000, 7, Cmd::Rotary(0));
        let sent = r.sent();
        assert_eq!(sent.len(), 2);
        let deltas: Vec<i32> = sent
            .iter()
            .map(|s| {
                let Sent::Msg(_, _, _, body) = s else {
                    panic!()
                };
                let r: proto::InputReport = proto::decode(body).unwrap();
                r.relative_event.unwrap().data[0].delta
            })
            .collect();
        assert_eq!(deltas, [-1, 3]);
        let mut contacts = [Contact::default(); MAX_SLOTS];
        contacts[0] = Contact {
            slot: 0,
            x: 5000,
            y: 5000,
            phase: Touch::Down,
        };
        r.m.on_cmd(2000, 7, Cmd::Touch { count: 1, contacts });
        let sent = r.sent();
        let Sent::Msg(_, _, _, body) = &sent[0] else {
            panic!()
        };
        let t: proto::InputReport = proto::decode(body).unwrap();
        assert_eq!(t.touch_event.unwrap().pointer_data[0].x, 400);
        r.m.on_cmd(
            2000,
            7,
            Cmd::Key {
                button: PhoneButton::Menu,
                down: true,
            },
        );
        assert!(r.sent().is_empty());
    }

    #[test]
    fn sensors_night_and_driving_status() {
        let mut r = Rig::new(Config {
            night: NightMode::Night,
            ..config()
        });
        r.connect();
        r.discover();
        r.sent();
        r.phone_sends_proto(
            CH_SENSOR,
            MSG_SENSOR_REQUEST,
            &proto::SensorRequest {
                sensor_type: SENSOR_NIGHT_MODE,
                min_update_period: 0,
            },
        )
        .unwrap();
        assert_eq!(
            r.sent(),
            [
                Sent::Msg(
                    CH_SENSOR,
                    Class::EncSignal,
                    MSG_SENSOR_RESPONSE,
                    vec![0x08, 0x00]
                ),
                Sent::Msg(
                    CH_SENSOR,
                    Class::EncSignal,
                    MSG_SENSOR_BATCH,
                    vec![0x52, 0x02, 0x08, 0x01]
                ),
            ]
        );
        r.phone_sends_proto(
            CH_SENSOR,
            MSG_SENSOR_REQUEST,
            &proto::SensorRequest {
                sensor_type: SENSOR_DRIVING_STATUS,
                min_update_period: 0,
            },
        )
        .unwrap();
        assert_eq!(
            ids(&r.sent()),
            [
                (CH_SENSOR, MSG_SENSOR_RESPONSE),
                (CH_SENSOR, MSG_SENSOR_BATCH)
            ]
        );
        r.m.on_cmd(2000, 0, Cmd::Night(NightMode::Day));
        assert_eq!(
            r.sent(),
            [Sent::Msg(
                CH_SENSOR,
                Class::EncSignal,
                MSG_SENSOR_BATCH,
                vec![0x52, 0x02, 0x08, 0x00]
            )]
        );
        r.m.on_cmd(2000, 0, Cmd::Night(NightMode::Auto));
        assert_eq!(r.sent().len(), 1);
        // The rest of the control channel.
        r.phone_sends_proto(
            CH_CONTROL,
            MSG_AUDIO_FOCUS_REQUEST,
            &proto::AudioFocusRequest {
                audio_focus_type: AUDIO_FOCUS_RELEASE,
            },
        )
        .unwrap();
        assert_eq!(
            r.sent(),
            [Sent::Msg(
                CH_CONTROL,
                Class::EncSignal,
                MSG_AUDIO_FOCUS_NOTIFICATION,
                vec![0x08, 0x03, 0x10, 0x00]
            )]
        );
        r.phone_sends_proto(
            CH_CONTROL,
            MSG_NAV_FOCUS_REQUEST,
            &proto::NavFocusRequestNotification {
                focus_type: Some(NAV_FOCUS_NATIVE),
            },
        )
        .unwrap();
        assert_eq!(
            r.sent(),
            [Sent::Msg(
                CH_CONTROL,
                Class::EncSignal,
                MSG_NAV_FOCUS_NOTIFICATION,
                vec![0x08, 0x02]
            )]
        );
        r.events();
        r.phone_sends(CH_CONTROL, Class::EncSignal, MSG_NAV_FOCUS_REQUEST, &[])
            .unwrap();
        r.phone_sends_proto(
            CH_CONTROL,
            MSG_VOICE_SESSION_NOTIFICATION,
            &proto::VoiceSessionNotification {
                status: Some(VOICE_SESSION_START),
            },
        )
        .unwrap();
        r.phone_sends_proto(
            CH_CONTROL,
            MSG_VOICE_SESSION_NOTIFICATION,
            &proto::VoiceSessionNotification {
                status: Some(VOICE_SESSION_END),
            },
        )
        .unwrap();
        assert_eq!(
            r.events(),
            [
                Event::NavFocus(true),
                Event::Assistant(true),
                Event::Assistant(false)
            ]
        );
        assert_eq!(ids(&r.sent()), [(CH_CONTROL, MSG_NAV_FOCUS_NOTIFICATION)]);
        r.phone_sends(CH_INPUT, Class::EncSignal, MSG_KEY_BINDING_REQUEST, &[])
            .unwrap();
        assert_eq!(ids(&r.sent()), [(CH_INPUT, MSG_KEY_BINDING_RESPONSE)]);
        // Unknown messages and channels are nothing.
        r.phone_sends(CH_CONTROL, Class::EncSignal, 0x7777, &[1])
            .unwrap();
        r.phone_sends(12, Class::EncSignal, 0x8001, &[1]).unwrap();
        r.phone_sends(CH_SENSOR, Class::EncSignal, 0x8009, &[])
            .unwrap();
        r.phone_sends(CH_INPUT, Class::EncSignal, 0x8009, &[])
            .unwrap();
        assert!(r.sent().is_empty());
        // A short message is not fatal.
        let h = Header {
            channel: CH_CONTROL,
            flags: FRAME_FIRST | FRAME_LAST,
            len: 1,
        };
        assert!(r.m.on_frame(2000, &h, 0, &[1]).unwrap().is_none());
    }

    #[test]
    fn bad_records_and_long_messages() {
        let mut r = Rig::new(config());
        r.go_live();
        r.sent();
        // Ciphertext that is not: fatal, as a desync would be.
        let h = Header {
            channel: CH_CONTROL,
            flags: 0x0b,
            len: 8,
        };
        let e = err(r.m.on_frame(2000, &h, 0, &[1, 2, 3, 4, 5, 6, 7, 8]));
        assert!(matches!(e, Fatal::Tls(_)));
        assert_eq!(e.fault(), Fault::LinkLost);
        // A message spanning frames comes through whole.
        let mut r = Rig::new(config());
        r.go_live();
        r.sent();
        let mut data = vec![0u8; 8];
        data.extend((0..50_000u32).map(|i| i as u8));
        let unit = r
            .phone_sends(CH_VIDEO, Class::EncSignal, MSG_MEDIA_DATA, &data)
            .unwrap()
            .unwrap();
        assert_eq!(unit.len(), 50_000);
        assert_eq!(unit.data()[49_999], (49_999u32) as u8);
    }

    #[test]
    fn config_pins_a_phone_or_takes_any() {
        let c = config();
        assert!(c.accepts(Id {
            vendor: 0x04e8,
            product: 0x6860
        }));
        let pinned = Config {
            vendor_id: 0x04e8,
            product_id: 0x6860,
            ..config()
        };
        assert!(pinned.accepts(Id {
            vendor: 0x04e8,
            product: 0x6860
        }));
        assert!(!pinned.accepts(Id {
            vendor: 0x18d1,
            product: 0x4ee1
        }));
        let vendor_only = Config {
            vendor_id: 0x04e8,
            ..config()
        };
        assert!(vendor_only.accepts(Id {
            vendor: 0x04e8,
            product: 1
        }));
        assert_eq!(c.offer.size(), (800, 480));
        assert_eq!(
            phase_from_u8(Phase::Backgrounded as u8),
            Phase::Backgrounded
        );
        assert_eq!(phase_from_u8(9), Phase::Closed);
        assert!(now_ns() > 0);
        let _ = Link::Usb;
    }
}
