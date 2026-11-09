// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
// Copyright (C) 2025-2026 Lasse Heitgres (LIVI, https://github.com/f-io/LIVI)
//! The AV session: what runs over the sealed control channel once the
//! phone has verified. The phone drives it with RTSP requests carrying
//! binary plists (`GET /info`, `SETUP`, `RECORD`, `/feedback`,
//! `/command`), then connects to the ports handed back: the screen over
//! TCP, each audio stream over RTP on UDP, the event channel for its
//! requests and our input, the timing sync, the iAP tunnel. Every stream's
//! key derives from the pair-verify secret.
//!
//! The receive paths allocate nothing per packet: frames are decrypted in
//! the buffer they landed in, or straight inside the access unit handed
//! to the decoder; audio is decrypted in place and handed to the mixer's
//! sink still encoded.
//!
//! The AV protocol here (the SETUP features, `/feedback`, the Siri and UI
//! requests, the screen header, the timing ports) follows LIVI's cp/stack:
//! cpStack.ts, screenStream.ts, audioStream.ts, timingServer.ts and
//! micUplink.ts (Lasse Heitgres, GPL-3.0), through FastCarPlay's
//! cp_av.cpp; the threads, the zero-copy paths and the call handling are
//! LEHU's.

use std::io::{self, Read, Write};
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, TcpListener, TcpStream, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lehu_core::clock;
use lehu_core::queue::Queue;
use lehu_session::{
    AccessUnit, AudioCodec, AudioFormat, AudioKind, Cmd, Event, MicFormat, PhoneButton, TOUCH_SCALE,
};
use lehu_video::Codec;
use lehu_video::annexb;

use crate::cipher::FrameCipher;
use crate::control::ConnHandle;
use crate::crypto::{self, ChaChaStream, TAG_LEN};
use crate::hid;
use crate::info::{self, PLIST_CT};
use crate::nalu;
use crate::plist::{self, Value};
use crate::rtsp::{self, Request, Response};
use crate::session::Shared;
use crate::state::Silence;
use crate::ui_sync::UiSync;
use crate::ultra;
use crate::vehicle::VehicleState;

/// The decoder's queue: wait this deep for this long before the oldest
/// frame goes, so the back-pressure reaches the phone over TCP and its
/// encoder skips frames, which breaks no reference chain.
const VIDEO_DEPTH: usize = 4;
const VIDEO_WAIT: Duration = Duration::from_millis(500);
/// The screen socket's receive buffer: small, so a stalled decoder stalls
/// the phone within a tenth of a second of video rather than after
/// megabytes of stale frames.
const SCREEN_RCVBUF: i32 = 64 * 1024;
const SCREEN_HEADER: usize = 128;
const OP_VIDEO_FRAME: u8 = 0;
const OP_VIDEO_CONFIG: u8 = 1;
const MAX_SCREEN_BODY: u32 = 8 * 1024 * 1024;
/// Event-channel bodies waiting to go: input and keyframe requests.
const OUTGOING_DEPTH: usize = 256;
const POLL: Duration = Duration::from_millis(300);
const PT_REQUEST: u8 = 210;
const PT_RESPONSE: u8 = 211;
/// A call whose voice stream falls silent this long missed its TEARDOWN.
const CALL_SILENT_MS: u64 = 3000;
/// The CarPlay Ultra data streams one session may set up.
const MAX_ULTRA_STREAMS: i64 = 8;

// ── Helpers ───────────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    clock::now_ms()
}

/// Now as a 64-bit NTP timestamp.
fn ntp_now() -> u64 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let sec = d.as_secs() + 2_208_988_800;
    let frac = (u64::from(d.subsec_nanos()) << 32) / 1_000_000_000;
    (sec << 32) | frac
}

// time_t is 32 bits on the board, 64 on the host: the casts are needed
// on one of them.
#[allow(clippy::unnecessary_cast)]
fn mono_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a plain clock read into a live struct.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as i64) * 1_000_000_000 + (ts.tv_nsec as i64)
}

/// Wait for `fd` to be readable, up to `timeout`.
fn readable(fd: i32, timeout: Duration) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd, live for the call.
    unsafe { libc::poll(&mut p, 1, timeout.as_millis() as i32) > 0 }
}

/// Dead-peer detection on a stream from the phone: a phone that vanishes
/// never sends FIN, and without this the connection stays established
/// with the last frame on the panel. 5 s idle, three probes 2 s apart.
pub fn keepalive(stream: &TcpStream) {
    let fd = stream.as_raw_fd();
    let set = |level, name, value: libc::c_int| {
        // SAFETY: setsockopt with an int of the stated size.
        unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                (&value as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 5);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 2);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3);
    let _ = stream.set_nodelay(true);
}

/// SCHED_FIFO for the audio threads, above the decoder: left at normal
/// priority the decoder starved them in bursts and the audio chopped.
/// Below the USB transport. Refused without the rtprio limit; then the
/// thread just runs normally.
fn realtime(shared: &Shared) {
    if !shared.config.realtime {
        return;
    }
    let param = libc::sched_param { sched_priority: 10 };
    // SAFETY: the param struct is live for the call; a failure is ignored.
    unsafe {
        libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param);
    }
}

fn bind_tcp() -> io::Result<TcpListener> {
    let l = TcpListener::bind((Ipv6Addr::UNSPECIFIED, 0))?;
    l.set_nonblocking(true)?;
    Ok(l)
}

fn bind_udp() -> io::Result<UdpSocket> {
    UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))
}

fn port_of(addr: io::Result<SocketAddr>) -> u16 {
    addr.map(|a| a.port()).unwrap_or(0)
}

fn pcm_rate(fmt: i64) -> u32 {
    if fmt & 0xc != 0 {
        8000
    } else if fmt & 0x30 != 0 {
        16000
    } else if fmt & 0xc0 != 0 {
        24000
    } else if fmt & 0x300 != 0 {
        32000
    } else if fmt & 0xc00 != 0 {
        44100
    } else if fmt & 0xc000 != 0 {
        48000
    } else {
        0
    }
}

fn is_mono_pcm(fmt: i64) -> bool {
    const MONO: i64 = 0x4 | 0x10 | 0x40 | 0x100 | 0x400 | 0x4000;
    const STEREO: i64 = 0x8 | 0x20 | 0x80 | 0x200 | 0x800 | 0x8000;
    fmt & MONO != 0 && fmt & STEREO == 0
}

/// The stream's coding from the format bit the phone picked. Telephony
/// and Siri downlink are mono PCM at 16 kHz; decoded as the old 44.1 kHz
/// stereo default they ran 2.75 times too fast.
pub fn audio_format_of(fmt: i64) -> AudioFormat {
    const AAC44: i64 = 0x40_0000;
    const AAC48: i64 = 0x80_0000;
    const OPUS: i64 = 0x7000_0000;
    if fmt & (AAC44 | AAC48) != 0 {
        AudioFormat {
            codec: AudioCodec::AacLc,
            rate: if fmt & AAC48 != 0 { 48000 } else { 44100 },
            channels: 2,
        }
    } else if fmt & OPUS != 0 {
        AudioFormat {
            codec: AudioCodec::Opus,
            rate: 48000,
            channels: 1,
        }
    } else {
        let rate = pcm_rate(fmt);
        AudioFormat {
            codec: AudioCodec::Pcm,
            rate: if rate > 0 { rate } else { 44100 },
            channels: if is_mono_pcm(fmt) { 1 } else { 2 },
        }
    }
}

/// What the mixer does with a stream, from its type and usage: music
/// rides the deep buffer, voice ducks it. Wired CarPlay ships music as
/// LPCM on the main audio stream with usage "media".
pub fn audio_kind_of(stream_type: i64, usage: &str) -> AudioKind {
    match (stream_type, usage) {
        (info::STREAM_MAIN_HIGH_AUDIO, _) | (_, "media") => AudioKind::Media,
        (_, "telephony") => AudioKind::Call,
        (_, "alert") => AudioKind::System,
        _ => AudioKind::Speech,
    }
}

/// A stream key: HKDF of the pair-verify secret with the stream id in the
/// salt. The id is a 64-bit value the phone renders unsigned, and it is
/// often above 2^63: rendered signed, the key is wrong and every packet
/// fails to decrypt.
pub fn stream_key(secret: &[u8; 32], stream_id: i64, info: &str) -> [u8; 32] {
    crypto::hkdf_sha512(
        secret,
        &format!("DataStream-Salt{}", stream_id as u64),
        info,
    )
}

// ── Shared state of one AV session ────────────────────────────────────────

/// The clock sync's numbers, under a lock: this core has no 64-bit
/// atomics.
struct TimingState {
    inner: Mutex<Timing>,
}

#[derive(Default)]
struct Timing {
    /// Our clock's offset onto the phone's NTP domain.
    offset: i64,
    /// When the phone last answered a timing request (`now_ms`), 0 never.
    last_heard: u64,
}

impl TimingState {
    fn lock(&self) -> MutexGuard<'_, Timing> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn synced(&self) -> u64 {
        (ntp_now() as i64).wrapping_add(self.lock().offset) as u64
    }

    fn steer(&self, offset: i64) {
        let mut t = self.lock();
        t.offset = t.offset.wrapping_add(offset);
        t.last_heard = now_ms();
    }

    fn last_heard(&self) -> u64 {
        self.lock().last_heard
    }
}

/// One audio stream as `/feedback` reports it. The phone treats a media
/// stream it does not see listed as dead and tears it down every few
/// seconds, which kept music on the phone instead of the car.
struct AudioMeta {
    stream_type: i64,
    stream_id: i64,
    rate: u32,
    /// Listed only once a packet arrived.
    first_ms: Option<u64>,
}

/// Where the microphone goes: the newest input stream's socket, address
/// and key. The phone sets up a fresh input stream for every call and
/// every Siri request; the newest takes the microphone.
struct Uplink {
    stream_id: i64,
    socket: Arc<UdpSocket>,
    peer: SocketAddrV6,
    /// The SETUP named the phone's port; without it the first downlink
    /// packet's source is taken whole.
    peer_seeded: bool,
    /// The address is taken from the first downlink packet: a SETUP seed
    /// gives the port, but the address and scope where audio really comes
    /// from are what routes back over USB (the control channel's peer did
    /// not).
    peer_latched: bool,
    payload_type: u8,
    aead: ChaChaStream,
    codec: AudioCodec,
    frame_samples: u32,
    seq: u16,
    timestamp: u32,
    nonce: u64,
    sent: u64,
    /// Frames sent and refused since the last report, which is due at
    /// `report_at` (every 5 s while frames flow).
    window_sent: u32,
    window_failed: u32,
    report_at: u64,
}

/// How often the microphone's numbers are logged while it runs.
const UPLINK_REPORT_MS: u64 = 5000;

struct Av {
    s: Arc<Shared>,
    handle: Arc<ConnHandle>,
    running: AtomicBool,
    /// RECORD seen: our event commands go only from then on; an early one
    /// stalled the bring-up on older phones.
    recorded: AtomicBool,
    secret: [u8; 32],
    peer: SocketAddrV6,
    timing: TimingState,
    audio: Mutex<Vec<AudioMeta>>,
    /// Telephony priority: while a call runs, media and alternate packets
    /// are dropped before decrypt so the voice has the core. Set at the
    /// telephony SETUP, cleared by its id-matched TEARDOWN, 3 s of silence
    /// on it, or the end.
    call_active: AtomicBool,
    /// The telephony and the speech-recognition stream ids, 0 for none.
    ids: Mutex<(i64, i64)>,
    uplink: Mutex<Option<Uplink>>,
    outgoing: Queue<Vec<u8>>,
    /// The stream's parameter sets, for a decoder that starts mid-stream
    /// (rebuilt after the camera had the memory): replayed before a
    /// keyframe is asked for, since the phone sends them only at the start.
    params: Mutex<Vec<u8>>,
    keyframe_pending: AtomicBool,
    /// When the keyframe was asked for, in ms (wrapping).
    keyframe_asked: AtomicU32,
    knob_select: AtomicBool,
    codec: AtomicU8,
    live_reported: AtomicBool,
}

impl Av {
    fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
            && self.s.is_running()
            && !self.handle.stop.load(Ordering::Acquire)
    }

    fn audio(&self) -> MutexGuard<'_, Vec<AudioMeta>> {
        self.audio.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn uplink(&self) -> MutexGuard<'_, Option<Uplink>> {
        self.uplink.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn call_stream_id(&self) -> i64 {
        self.ids.lock().unwrap_or_else(|e| e.into_inner()).0
    }

    fn set_call_stream(&self, id: i64) {
        self.ids.lock().unwrap_or_else(|e| e.into_inner()).0 = id;
    }

    fn assistant_id(&self) -> i64 {
        self.ids.lock().unwrap_or_else(|e| e.into_inner()).1
    }

    fn set_assistant(&self, id: i64) {
        self.ids.lock().unwrap_or_else(|e| e.into_inner()).1 = id;
    }

    fn hevc(&self) -> bool {
        self.codec.load(Ordering::Relaxed) == 2
    }

    fn queue_command(&self, body: Vec<u8>) {
        self.outgoing.push_drop_oldest(body);
        self.s.cmd_wake.wake();
    }

    /// The last parameter sets in front of the keyframe about to come.
    fn replay_params(&self) {
        let params = self.params.lock().unwrap_or_else(|e| e.into_inner());
        if params.is_empty() {
            return;
        }
        let mut unit = self
            .s
            .ports
            .recycle
            .pop()
            .unwrap_or_else(|| AccessUnit::with_capacity(self.s.config.unit_bytes));
        unit.clear();
        if unit.write(&params) {
            unit.config = true;
            self.s.ports.video.push_drop_oldest(unit);
        }
    }

    fn end_call(&self, why: &str) {
        if self.call_active.swap(false, Ordering::AcqRel) {
            self.set_call_stream(0);
            log::info!("cp-av: call mode off ({why})");
            self.s.report(Event::Call(false));
        }
    }

    fn end_assistant(&self) {
        let had = self.assistant_id();
        self.set_assistant(0);
        if had != 0 {
            self.s.report(Event::Assistant(false));
        }
    }

    /// The input stream `stream_id` is gone: the microphone with it, when
    /// it was the newest.
    fn drop_uplink(&self, stream_id: i64) {
        let mut u = self.uplink();
        if u.as_ref().is_some_and(|u| u.stream_id == stream_id) {
            *u = None;
            drop(u);
            self.s.sinks().mic(None);
        }
    }
}

// ── The session ───────────────────────────────────────────────────────────

struct Worker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn join(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub struct AvSession {
    av: Arc<Av>,
    workers: Vec<Worker>,
    /// Audio stream workers by stream id, so TEARDOWN ends them.
    streams: Vec<(i64, Worker)>,
    silence: Silence,
    /// CarPlay Ultra: the vehicle state both its streams share, and what
    /// the session SETUP enabled.
    vehicle: Option<Arc<Mutex<VehicleState>>>,
    ultra: ultra::Enabled,
    /// The last data stream id handed out; the iAP tunnel's is 1.
    data_stream_id: i64,
}

impl AvSession {
    pub fn new(
        shared: Arc<Shared>,
        handle: Arc<ConnHandle>,
        secret: [u8; 32],
        peer: SocketAddrV6,
    ) -> Self {
        let av = Arc::new(Av {
            s: shared,
            handle,
            running: AtomicBool::new(true),
            recorded: AtomicBool::new(false),
            secret,
            peer,
            timing: TimingState {
                inner: Mutex::new(Timing::default()),
            },
            audio: Mutex::new(Vec::new()),
            call_active: AtomicBool::new(false),
            ids: Mutex::new((0, 0)),
            uplink: Mutex::new(None),
            outgoing: Queue::new(OUTGOING_DEPTH),
            params: Mutex::new(Vec::new()),
            keyframe_pending: AtomicBool::new(false),
            keyframe_asked: AtomicU32::new(0),
            knob_select: AtomicBool::new(false),
            codec: AtomicU8::new(0),
            live_reported: AtomicBool::new(false),
        });
        // Commands queued while no session could send them belong to it
        // no more.
        av.s.cmds.clear();
        let vehicle = av.s.config.ultra.then(|| {
            Arc::new(Mutex::new(VehicleState::new(
                &av.s.config.name,
                env!("CARGO_PKG_VERSION"),
            )))
        });
        Self {
            av,
            workers: Vec::new(),
            streams: Vec::new(),
            silence: Silence::new(now_ms()),
            vehicle,
            ultra: ultra::Enabled::default(),
            data_stream_id: 1,
        }
    }

    /// The phone has gone quiet past the limit.
    pub fn silent(&mut self) -> bool {
        let heard = self.av.timing.last_heard();
        if heard > 0 {
            self.silence.timing(heard);
        }
        self.silence.silent(now_ms())
    }

    fn spawn(
        &mut self,
        name: &str,
        f: impl FnOnce(Arc<AtomicBool>) + Send + 'static,
    ) -> Option<Worker> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        match thread::Builder::new()
            .name(name.into())
            .spawn(move || f(flag))
        {
            Ok(t) => Some(Worker {
                stop,
                thread: Some(t),
            }),
            Err(e) => {
                log::error!("cp-av: cannot start {name}: {e}");
                None
            }
        }
    }

    /// A TCP listener whose first client is served by `on_client` on the
    /// listener's thread, one at a time, as the phone connects once.
    fn listen(
        &mut self,
        name: &'static str,
        on_client: impl Fn(&Arc<Av>, TcpStream, &AtomicBool) + Send + 'static,
    ) -> u16 {
        let Ok(listener) = bind_tcp() else {
            return 0;
        };
        let port = port_of(listener.local_addr());
        let av = Arc::clone(&self.av);
        let worker = self.spawn(name, move |stop| {
            while av.running() && !stop.load(Ordering::Acquire) {
                if !readable(listener.as_raw_fd(), POLL) {
                    continue;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        keepalive(&stream);
                        log::info!("cp-av: {name} connected");
                        on_client(&av, stream, &stop);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        log::warn!("cp-av: {name} accept: {e}");
                        break;
                    }
                }
            }
        });
        match worker {
            Some(w) => {
                self.workers.push(w);
                port
            }
            None => 0,
        }
    }

    /// One AV request; `None` when it is not one of ours.
    pub fn handle(&mut self, req: &Request) -> Option<Response> {
        self.silence.request(now_ms());
        let path = req.path_only();
        let method = req.method.as_str();
        Some(match (method, path) {
            ("GET", p) if p.ends_with("/info") => self.handle_info(),
            ("SETUP", _) => self.handle_setup(req),
            ("RECORD", _) => {
                log::info!("cp-av: RECORD, the session starts");
                self.av.recorded.store(true, Ordering::Release);
                self.av.s.cmd_wake.wake();
                Response::ok()
            }
            ("FLUSH", _) => {
                // The phone dropped the media it sent ahead (a pause, a
                // skip): what is queued must not play.
                log::info!("cp-av: FLUSH");
                self.av.s.report(Event::AudioFlush);
                Response::ok()
            }
            ("SET_PARAMETER" | "GET_PARAMETER" | "OPTIONS", _) => Response::ok(),
            ("TEARDOWN", _) => self.handle_teardown(req),
            ("POST", p) if p.ends_with("/feedback") => self.handle_feedback(),
            ("POST", p) if p.ends_with("/command") => {
                self.handle_command(req);
                Response::ok()
            }
            _ => return None,
        })
    }

    fn handle_info(&self) -> Response {
        let cfg = &self.av.s.config;
        let body = match &self.vehicle {
            Some(vehicle) => {
                let mut v = info::info_value(cfg, cfg.night);
                let vehicle = vehicle.lock().unwrap_or_else(|e| e.into_inner());
                let themed = cfg.ultra_theme().is_some();
                ultra::add_info(&mut v, &vehicle, themed, self.ultra.ui_sync);
                plist::encode(&v)
            }
            None => info::info_plist(cfg, cfg.night),
        };
        log::info!(
            "cp-av: GET /info, {} bytes{}",
            body.len(),
            if self.vehicle.is_some() {
                ", with the Ultra panels"
            } else {
                ""
            }
        );
        Response::ok().with_body(PLIST_CT, body)
    }

    fn handle_command(&self, req: &Request) {
        let Some(cmd) = plist::decode(&req.body) else {
            return;
        };
        // The dock's car icon: the phone asks for the head unit's own UI.
        if cmd.str_or("type", "") == "requestUI" {
            log::info!("cp-av: the phone asks for the head unit's UI");
            self.av.s.report(Event::HostUi);
        }
    }

    fn handle_teardown(&mut self, req: &Request) -> Response {
        let body = plist::decode(&req.body).unwrap_or_else(Value::dict);
        match body.get("streams").and_then(Value::as_array) {
            Some(streams) => {
                for s in streams {
                    let id = s.int_or("streamConnectionID", 0);
                    let stream_type = s.int_or("type", 0);
                    log::info!("cp-av: TEARDOWN stream {} (type {stream_type})", id as u64);
                    if id == 0 {
                        // A wired phone names the type alone: at the end of a
                        // call, and when music pauses. The streams stay (the
                        // phone sends on them again when the music goes on);
                        // a call ends, its microphone with it.
                        self.end_by_type();
                    } else {
                        self.end_stream(id);
                    }
                }
            }
            None => {
                log::info!("cp-av: TEARDOWN of the session");
                let ids: Vec<i64> = self.streams.iter().map(|(id, _)| *id).collect();
                for id in ids {
                    self.end_stream(id);
                }
                self.av.end_call("teardown");
                self.av.end_assistant();
            }
        }
        Response::ok()
    }

    /// A TEARDOWN that names no stream: call mode ends, and the call's
    /// microphone. The assistant is left alone: the phone pauses music for
    /// Siri, and that pause would end it.
    fn end_by_type(&mut self) {
        let call = self.av.call_stream_id();
        self.av.end_call("teardown");
        if call != 0 {
            self.av.drop_uplink(call);
        }
    }

    /// End one audio stream: its thread, its feedback entry, its call or
    /// assistant state. Matched by id, so the phone's set-up-new-then-tear
    /// -down-old churn cannot end a live call.
    fn end_stream(&mut self, id: i64) {
        if let Some(pos) = self.streams.iter().position(|(sid, _)| *sid == id) {
            let (_, w) = self.streams.remove(pos);
            w.stop.store(true, Ordering::Release);
            self.workers.push(w);
        }
        self.av.audio().retain(|m| m.stream_id != id);
        if id == 0 || self.av.call_stream_id() == id {
            self.av.end_call("teardown");
        }
        if self.av.assistant_id() == id {
            self.av.end_assistant();
        }
        self.av.drop_uplink(id);
    }

    /// Every live audio stream: type, rate, id, and the sample the speaker
    /// should be playing now against the synced clock.
    fn handle_feedback(&self) -> Response {
        let now = now_ms();
        let ntp = self.av.timing.synced();
        let raw = mono_ns();
        let streams: Vec<Value> = self
            .av
            .audio()
            .iter()
            .filter_map(|m| {
                let first = m.first_ms?;
                let played = now.saturating_sub(first) * u64::from(m.rate) / 1000;
                Some(
                    Value::dict()
                        .with("type", Value::Int(m.stream_type))
                        .with("sampleRate", Value::Int(i64::from(m.rate)))
                        .with("streamConnectionID", Value::Int(m.stream_id))
                        .with("timestamp", Value::Int(ntp as i64))
                        .with("timestampRawNs", Value::Int(raw))
                        .with("sampleTime", Value::Int(played as i64)),
                )
            })
            .collect();
        let body = plist::encode(&Value::dict().with("streams", Value::Array(streams)));
        Response::ok().with_body(PLIST_CT, body)
    }

    fn handle_setup(&mut self, req: &Request) -> Response {
        let body = plist::decode(&req.body).unwrap_or_else(Value::dict);
        if let Some(streams) = body.get("streams").and_then(Value::as_array) {
            let mut out = Vec::new();
            for s in streams {
                if let Some(entry) = self.setup_stream(s) {
                    out.push(entry);
                }
            }
            let resp = plist::encode(&Value::dict().with("streams", Value::Array(out)));
            return Response::ok().with_body(PLIST_CT, resp);
        }
        // The session-level SETUP: the event channel, the timing sync and
        // the low-power keep-alive port the phone asks for.
        if let Some(name) = body.get("name").and_then(Value::as_str)
            && !name.is_empty()
            && self.av.s.phone_name.set(name.to_string()).is_ok()
        {
            self.av.s.report(Event::Phase(self.av.s.phase()));
        }
        let event_port = self.listen("cp-event", event_loop);
        let timing_port = self.start_timing(body.int_or("timingPort", 0) as u16);
        let mut resp = Value::dict()
            .with("eventPort", Value::Int(i64::from(event_port)))
            .with("timingPort", Value::Int(i64::from(timing_port)));
        if body.bool_of("keepAliveLowPower") == Some(true) {
            // A UDP port that absorbs the phone's keep-alive datagrams. A
            // TCP listener here made them bounce and the phone leave.
            if let Ok(socket) = bind_udp() {
                let port = port_of(socket.local_addr());
                let _ = socket.set_read_timeout(Some(POLL));
                let av = Arc::clone(&self.av);
                if let Some(w) = self.spawn("cp-keepalive", move |stop| {
                    let mut b = [0u8; 256];
                    while av.running() && !stop.load(Ordering::Acquire) {
                        let _ = socket.recv_from(&mut b);
                    }
                }) {
                    self.workers.push(w);
                    resp.set("keepAlivePort", Value::Int(i64::from(port)));
                }
            }
        }
        let mut features = Vec::new();
        if self.av.s.config.codec.is_hevc() {
            features.push(Value::str("hevc"));
        }
        features.push(Value::str("iAPChannel"));
        features.push(Value::str("viewAreas"));
        let offered: Vec<&str> = body
            .get("features")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        if !offered.is_empty() {
            log::info!("cp-av: the phone offers {}", offered.join(", "));
        }
        if self.vehicle.is_some() {
            let themed = self.av.s.config.ultra_theme().is_some();
            self.ultra = ultra::Enabled::from_offer(&offered, themed);
            features.extend(self.ultra.names().map(Value::str));
        }
        resp.set("enabledFeatures", Value::Array(features));
        log::info!(
            "cp-av: session SETUP: event {event_port}, timing {timing_port}, phone {} ({})",
            body.str_or("name", "?"),
            body.str_or("model", "?")
        );
        Response::ok().with_body(PLIST_CT, plist::encode(&resp))
    }

    fn setup_stream(&mut self, s: &Value) -> Option<Value> {
        let stream_type = s.int_or("type", 0);
        let id = s.int_or("streamConnectionID", 0);
        let mut entry = Value::dict().with("type", Value::Int(stream_type));
        match stream_type {
            // No alternate screen is declared, and the instrument panel has
            // no stream: one asked for anyway would take the one decoder.
            info::STREAM_ALT_SCREEN if self.vehicle.is_some() => {
                log::warn!(
                    "cp-av: the phone asks for a second screen; this head unit has one display \
                     and one decoder, refused"
                );
                return None;
            }
            info::STREAM_MAIN_SCREEN | info::STREAM_ALT_SCREEN => {
                let port = self.listen("cp-screen", move |av, stream, stop| {
                    screen_loop(av, stream, id, stop)
                });
                if port == 0 {
                    return None;
                }
                entry.set("dataPort", Value::Int(i64::from(port)));
                log::info!(
                    "cp-av: SETUP screen {stream_type} on {port}, stream {}",
                    id as u64
                );
            }
            info::STREAM_MAIN_AUDIO | info::STREAM_ALT_AUDIO | info::STREAM_MAIN_HIGH_AUDIO => {
                let (data_port, ctrl_port) = self.setup_audio(s, stream_type, id)?;
                entry.set("dataPort", Value::Int(i64::from(data_port)));
                entry.set("controlPort", Value::Int(i64::from(ctrl_port)));
                entry.set("streamConnectionID", Value::Int(id));
            }
            info::STREAM_DATA => {
                if let Some(channel) =
                    ultra::Channel::of(s.str_or("clientTypeUUID", ""), self.ultra)
                {
                    return self.setup_ultra_stream(s, channel);
                }
                let seed = s.int_or("seed", 0);
                let port = self.listen("cp-tunnel", move |av, stream, stop| {
                    tunnel_loop(av, stream, seed, stop)
                });
                if port == 0 {
                    return None;
                }
                entry.set("streamID", Value::Int(1));
                entry.set("dataPort", Value::Int(i64::from(port)));
                log::info!("cp-av: SETUP iAP tunnel on {port}");
            }
            other => {
                log::warn!("cp-av: stream type {other} not handled");
                return None;
            }
        }
        Some(entry)
    }

    /// A CarPlay Ultra data stream: the vehicle state (two of them, one
    /// state) or the UI sync, each served on its listener's thread.
    fn setup_ultra_stream(&mut self, s: &Value, channel: ultra::Channel) -> Option<Value> {
        let seed = s.int_or("seed", 0);
        let name = channel.name();
        // Three in a session; a phone setting up more is refused rather
        // than given a thread each.
        if self.data_stream_id > MAX_ULTRA_STREAMS {
            log::warn!("cp-av: SETUP {name} refused, {MAX_ULTRA_STREAMS} streams already");
            return None;
        }
        let port = match channel {
            ultra::Channel::VehicleState { .. } => {
                let vehicle = Arc::clone(self.vehicle.as_ref()?);
                // Classic CarPlay never opens it: the phone runs Ultra.
                self.av.s.ultra.store(true, Ordering::Release);
                self.listen("cp-vehicle", move |av, stream, stop| {
                    let alive = || av.running() && !stop.load(Ordering::Acquire);
                    let mut handle =
                        |m: &[u8]| vehicle.lock().unwrap_or_else(|e| e.into_inner()).handle(m);
                    ultra::serve(stream, &av.secret, seed, name, &alive, &mut handle);
                })
            }
            ultra::Channel::UiSync => {
                let layout = self
                    .av
                    .s
                    .config
                    .ultra_theme()
                    .and_then(|t| t.layout.clone());
                self.listen("cp-ui-sync", move |av, stream, stop| {
                    let alive = || av.running() && !stop.load(Ordering::Acquire);
                    let mut sync = UiSync::new(layout.clone());
                    let mut handle = |m: &[u8]| sync.handle(m);
                    ultra::serve(stream, &av.secret, seed, name, &alive, &mut handle);
                })
            }
        };
        if port == 0 {
            return None;
        }
        self.data_stream_id += 1;
        log::info!(
            "cp-av: SETUP {name} on {port}, stream {}",
            self.data_stream_id
        );
        Some(
            Value::dict()
                .with("type", Value::Int(info::STREAM_DATA))
                .with("streamID", Value::Int(self.data_stream_id))
                .with("dataPort", Value::Int(i64::from(port))),
        )
    }

    fn setup_audio(&mut self, s: &Value, stream_type: i64, id: i64) -> Option<(u16, u16)> {
        let fmt = s.int_or("audioFormat", 0);
        let usage = s.str_or("audioType", "").to_string();
        let format = audio_format_of(fmt);
        let kind = audio_kind_of(stream_type, &usage);
        let data = Arc::new(bind_udp().ok()?);
        let ctrl = bind_udp().ok()?;
        let (data_port, ctrl_port) = (port_of(data.local_addr()), port_of(ctrl.local_addr()));
        let _ = data.set_read_timeout(Some(POLL));
        let _ = ctrl.set_nonblocking(true);
        // An input-capable stream carries the "input" flag; without it
        // (older phones) the usage says, then the format.
        let input_capable = |u: &str| {
            matches!(
                u,
                "telephony" | "speechRecognition" | "default" | "compatibility"
            )
        };
        let wants_mic = stream_type == info::STREAM_MAIN_AUDIO
            && match s.bool_of("input") {
                Some(flag) => flag,
                None if usage.is_empty() => is_mono_pcm(fmt),
                None => input_capable(&usage),
            };
        // Reap workers whose threads have finished, so a long session with
        // the phone's re-SETUP churn does not pile up handles.
        self.workers.retain_mut(|w| match &w.thread {
            Some(t) if t.is_finished() => {
                w.join();
                false
            }
            _ => true,
        });
        let stop = Arc::new(AtomicBool::new(false));
        self.av
            .audio()
            .retain(|m| m.stream_type != stream_type || m.stream_id == id);
        self.av.audio().push(AudioMeta {
            stream_type,
            stream_id: id,
            rate: format.rate,
            first_ms: None,
        });
        if stream_type == info::STREAM_MAIN_AUDIO && usage == "telephony" {
            self.av.set_call_stream(id);
            if !self.av.call_active.swap(true, Ordering::AcqRel) {
                log::info!("cp-av: call mode on (stream {})", id as u64);
                self.av.s.report(Event::Call(true));
            }
        }
        if stream_type == info::STREAM_MAIN_AUDIO && usage == "speechRecognition" {
            self.av.set_assistant(id);
            self.av.s.report(Event::Assistant(true));
        }
        if wants_mic {
            self.setup_uplink(s, fmt, id, Arc::clone(&data));
        }
        let av = Arc::clone(&self.av);
        let flag = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("cp-audio".into())
            .spawn(move || {
                audio_loop(
                    &av,
                    &data,
                    &ctrl,
                    id,
                    stream_type,
                    kind,
                    format,
                    wants_mic,
                    &flag,
                )
            })
            .ok()?;
        self.streams.push((
            id,
            Worker {
                stop,
                thread: Some(thread),
            },
        ));
        log::info!(
            "cp-av: SETUP audio {stream_type} {usage} format {fmt:#x} ({:?} {} Hz {}ch) on {data_port}/{ctrl_port}, stream {}, mic {wants_mic}",
            format.codec,
            format.rate,
            format.channels,
            id as u64
        );
        Some((data_port, ctrl_port))
    }

    /// The microphone to this stream: its coding and rate from the format
    /// bit, the phone's address seeded from the SETUP (Siri listens first
    /// and sends nothing down until it hears us, so waiting for a downlink
    /// packet to learn the address would wait forever) and refreshed from
    /// the first downlink packet. The newest input stream supersedes.
    fn setup_uplink(&mut self, s: &Value, fmt: i64, id: i64, socket: Arc<UdpSocket>) {
        const OPUS: i64 = 0x7000_0000;
        let (codec, rate) = if fmt & OPUS != 0 {
            let rate = if fmt & 0x4000_0000 != 0 {
                48000
            } else if fmt & 0x2000_0000 != 0 {
                24000
            } else {
                16000
            };
            (AudioCodec::Opus, rate)
        } else {
            let r = pcm_rate(fmt);
            (AudioCodec::Pcm, if r > 0 { r } else { 16000 })
        };
        // LPCM goes in the phone's own packet size, 7.5 ms (360 samples at
        // 48 kHz): 20 ms packets were discarded in silence, the caller
        // heard nothing. Opus keeps 20 ms, a valid frame length.
        let (frame_tenths_ms, frame_samples) = match codec {
            AudioCodec::Pcm => (75, rate * 3 / 400),
            _ => (200, rate / 50),
        };
        let mic_port = s.int_or("dataPort", 0);
        let mut peer = self.av.peer;
        let peer_seeded = (1..=65535).contains(&mic_port);
        if peer_seeded {
            peer.set_port(mic_port as u16);
        }
        let key = stream_key(&self.av.secret, id, "DataStream-Input-Encryption-Key");
        let uplink = Uplink {
            stream_id: id,
            socket,
            peer,
            peer_seeded,
            peer_latched: false,
            payload_type: (id as u8) & 0x7f,
            aead: ChaChaStream::new(&key),
            codec,
            frame_samples,
            seq: 0,
            timestamp: 0,
            nonce: 0,
            sent: 0,
            window_sent: 0,
            window_failed: 0,
            report_at: now_ms() + UPLINK_REPORT_MS,
        };
        *self.av.uplink() = Some(uplink);
        self.av.s.sinks().mic(Some(MicFormat {
            codec,
            rate,
            frame_tenths_ms,
        }));
        if !self
            .workers
            .iter()
            .any(|w| w.thread.as_ref().and_then(|t| t.thread().name()) == Some("cp-mic"))
        {
            let av = Arc::clone(&self.av);
            if let Some(w) = self.spawn("cp-mic", move |stop| uplink_loop(&av, &stop)) {
                self.workers.push(w);
            }
        }
        log::info!(
            "cp-av: microphone to stream {} ({codec:?} {rate} Hz, {frame_samples}-sample frames)",
            id as u64
        );
    }

    fn start_timing(&mut self, phone_port: u16) -> u16 {
        let Ok(socket) = bind_udp() else {
            return 0;
        };
        let port = port_of(socket.local_addr());
        let _ = socket.set_read_timeout(Some(Duration::from_millis(200)));
        let av = Arc::clone(&self.av);
        if let Some(w) = self.spawn("cp-timing", move |stop| {
            timing_loop(&av, &socket, phone_port, &stop)
        }) {
            self.workers.push(w);
        }
        log::info!("cp-av: timing on {port} to the phone's {phone_port}");
        port
    }

    /// End everything: the streams' threads first (they exit within a
    /// poll), then the sockets with them.
    pub fn stop(&mut self) {
        self.av.running.store(false, Ordering::Release);
        self.av.s.cmd_wake.wake();
        for (_, w) in &mut self.streams {
            w.join();
        }
        for w in &mut self.workers {
            w.join();
        }
        self.streams.clear();
        self.workers.clear();
        self.av.end_call("session end");
        self.av.end_assistant();
        if self.av.uplink().take().is_some() {
            self.av.s.sinks().mic(None);
        }
        self.av.audio().clear();
    }
}

impl Drop for AvSession {
    fn drop(&mut self) {
        self.stop();
    }
}

// ── The loop's commands as event-channel bodies ───────────────────────────

fn translate(av: &Arc<Av>, cmd: Cmd, out: &mut Vec<Vec<u8>>) {
    let s = &av.s;
    let (w, h) = (s.config.width, s.config.height);
    let knob = |select: bool, home: bool, back: bool, x: i8, y: i8, wheel: i8| {
        info::hid_command(
            hid::KNOB_UUID,
            &hid::knob_report(select, home, back, x, y, wheel),
        )
    };
    let select = av.knob_select.load(Ordering::Relaxed);
    match cmd {
        Cmd::Touch { count, contacts } => {
            let mut hc = [hid::Contact::default(); hid::TOUCH_CONTACTS];
            let n = usize::from(count).min(hid::TOUCH_CONTACTS);
            for (i, c) in contacts.iter().take(n).enumerate() {
                hc[i] = hid::Contact {
                    slot: i as u8,
                    x: (u32::from(c.x) * w / u32::from(TOUCH_SCALE)).min(w) as u16,
                    y: (u32::from(c.y) * h / u32::from(TOUCH_SCALE)).min(h) as u16,
                    down: c.phase != lehu_input::Phase::Up,
                };
            }
            out.push(info::hid_command(
                hid::TOUCH_UUID,
                &hid::touch_report(&hc[..n]),
            ));
        }
        Cmd::Key { button, down } => {
            let media = |key: hid::Media, out: &mut Vec<Vec<u8>>| {
                out.push(info::hid_command(hid::MEDIA_UUID, &hid::media_report(key)));
                out.push(info::hid_command(
                    hid::MEDIA_UUID,
                    &hid::media_report(hid::Media::None),
                ));
            };
            let tap = |home: bool, back: bool, out: &mut Vec<Vec<u8>>| {
                out.push(knob(select, home, back, 0, 0, 0));
                out.push(knob(select, false, false, 0, 0, 0));
            };
            let mv = |x: i8, y: i8, out: &mut Vec<Vec<u8>>| {
                out.push(knob(select, false, false, x, y, 0));
                out.push(knob(select, false, false, 0, 0, 0));
            };
            match (button, down) {
                // The knob's press is held until its release: every knob
                // report carries the whole state.
                (PhoneButton::Select, d) => {
                    av.knob_select.store(d, Ordering::Relaxed);
                    out.push(knob(d, false, false, 0, 0, 0));
                }
                (_, false) => {}
                (PhoneButton::Home, _) => tap(true, false, out),
                (PhoneButton::Back, _) => tap(false, true, out),
                (PhoneButton::Left, _) => mv(-127, 0, out),
                (PhoneButton::Right, _) => mv(127, 0, out),
                (PhoneButton::Up, _) => mv(0, -127, out),
                (PhoneButton::Down, _) => mv(0, 127, out),
                (PhoneButton::Play, _) => media(hid::Media::Play, out),
                (PhoneButton::Pause, _) => media(hid::Media::Pause, out),
                (PhoneButton::PlayPause, _) => media(hid::Media::PlayPause, out),
                (PhoneButton::NextTrack, _) => media(hid::Media::Next, out),
                (PhoneButton::PrevTrack, _) => media(hid::Media::Prev, out),
                (PhoneButton::Siri, _) => {
                    out.push(info::siri_command(2));
                    out.push(info::siri_command(3));
                }
                (PhoneButton::RotateNext, _) => translate(av, Cmd::Rotary(1), out),
                (PhoneButton::RotatePrev, _) => translate(av, Cmd::Rotary(-1), out),
                (PhoneButton::Menu, _) => {}
            }
        }
        Cmd::Rotary(n) => {
            // One count per detent, +1 clockwise; a burst is at most eight.
            let step: i8 = if n < 0 { -1 } else { 1 };
            for _ in 0..n.unsigned_abs().min(8) {
                out.push(knob(select, false, false, 0, 0, step));
                out.push(knob(select, false, false, 0, 0, 0));
            }
        }
        Cmd::Keyframe | Cmd::Resume => {
            av.replay_params();
            out.push(info::force_keyframe_command(info::main_uuid(&s.config)));
        }
        Cmd::VideoActive(on) => {
            if on {
                av.replay_params();
                out.push(info::force_keyframe_command(info::main_uuid(&s.config)));
                if av.codec.load(Ordering::Relaxed) != 0 && av.recorded.load(Ordering::Acquire) {
                    s.set_phase(lehu_session::Phase::Live);
                }
            } else if s.phase() == lehu_session::Phase::Live {
                s.set_phase(lehu_session::Phase::Backgrounded);
            }
        }
        Cmd::Night(mode) => {
            let [a, b] = info::appearance_commands(info::main_uuid(&s.config), mode);
            out.push(a);
            out.push(b);
        }
        Cmd::Close => {}
    }
}

// ── The event channel ─────────────────────────────────────────────────────

/// The phone's reverse-HTTP requests, each answered 200 or the session
/// stalls, and our commands, from RECORD on. The keys are not swapped: we
/// write with Events-Write, read with Events-Read.
fn event_loop(av: &Arc<Av>, mut stream: TcpStream, stop: &AtomicBool) {
    let write_key = crypto::hkdf_sha512(&av.secret, "Events-Salt", "Events-Write-Encryption-Key");
    let read_key = crypto::hkdf_sha512(&av.secret, "Events-Salt", "Events-Read-Encryption-Key");
    let mut cipher = FrameCipher::new(&read_key, &write_key);
    let _ = stream.set_nonblocking(true);
    let mut sealed = Vec::with_capacity(4096);
    let mut plain = Vec::with_capacity(4096);
    let mut out = Vec::with_capacity(4096);
    let mut buf = [0u8; 4096];
    let mut cseq = 0u32;
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    while av.running() && !stop.load(Ordering::Acquire) {
        let mut fds = [
            libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: av.s.cmd_wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two live pollfds.
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, POLL.as_millis() as i32) };
        if r < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            break;
        }
        if fds[1].revents != 0 {
            av.s.cmd_wake.drain();
        }
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match stream.read(&mut buf) {
                Ok(0) => {
                    log::info!("cp-av: event channel closed");
                    break;
                }
                Ok(n) => {
                    sealed.extend_from_slice(&buf[..n]);
                    if !cipher.decrypt(&mut sealed, &mut plain) {
                        log::warn!("cp-av: event channel decrypt failed");
                        av.handle.stop.store(true, Ordering::Release);
                        break;
                    }
                    let Some(reqs) = rtsp::parse(&mut plain) else {
                        log::warn!("cp-av: event channel framing broken");
                        break;
                    };
                    out.clear();
                    for req in &reqs {
                        if req.is_response() {
                            continue;
                        }
                        log::debug!(
                            "cp-av: event {} {} ({} bytes)",
                            req.method,
                            req.path,
                            req.body.len()
                        );
                        cipher.encrypt_into(&rtsp::build(req, &Response::ok()), &mut out);
                    }
                    if !out.is_empty() && stream.write_all(&out).is_err() {
                        break;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => {
                    log::info!("cp-av: event channel: {e}");
                    break;
                }
            }
        }
        if !av.recorded.load(Ordering::Acquire) {
            continue;
        }
        bodies.clear();
        while let Some(cmd) = av.s.cmds.pop() {
            translate(av, cmd, &mut bodies);
        }
        while let Some(body) = av.outgoing.pop() {
            bodies.push(body);
        }
        if bodies.is_empty() {
            continue;
        }
        out.clear();
        for body in &bodies {
            cseq += 1;
            let req = rtsp::build_request("POST", "/command", cseq, PLIST_CT, body);
            cipher.encrypt_into(&req, &mut out);
        }
        if stream.write_all(&out).is_err() {
            break;
        }
    }
    log::debug!("cp-av: event channel ended");
}

// ── The screen ────────────────────────────────────────────────────────────

/// The phone sends messages of a 128-byte header and a body: body size
/// (u32 LE) at 0, the opcode at 4. Opcode 1 is the codec configuration in
/// the clear; opcode 0 a frame sealed with the header as the associated
/// data and the frame counter as the nonce, its plaintext length-prefixed
/// NAL units; opcode 2 a keep-alive with nothing. The counter advances only
/// on frames.
/// The screen stream's reassembly buffer: frames are read into it and
/// decrypted out of it, so it holds about one frame at a time.
const ACC_BYTES: usize = 256 * 1024;

fn screen_loop(av: &Arc<Av>, mut stream: TcpStream, stream_id: i64, stop: &AtomicBool) {
    let s = &av.s;
    let key = stream_key(&av.secret, stream_id, "DataStream-Output-Encryption-Key");
    let aead = ChaChaStream::new(&key);
    // SAFETY: setsockopt with an int.
    unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&SCREEN_RCVBUF as *const i32).cast(),
            4,
        );
    }
    let _ = stream.set_read_timeout(Some(POLL));
    let mut acc: Vec<u8> = Vec::with_capacity(ACC_BYTES);
    let mut counter = 0u64;
    let (mut frames, mut configs, mut dropped) = (0u64, 0u64, 0u64);
    let cap = s.config.unit_bytes;
    let take_unit = || {
        let mut u = s
            .ports
            .recycle
            .pop()
            .unwrap_or_else(|| AccessUnit::with_capacity(cap));
        u.clear();
        u
    };
    'stream: while av.running() && !stop.load(Ordering::Acquire) {
        let have = acc.len();
        acc.resize(have + 32 * 1024, 0);
        let n = match stream.read(&mut acc[have..]) {
            Ok(0) => {
                acc.truncate(have);
                break;
            }
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                acc.truncate(have);
                continue;
            }
            Err(e) => {
                log::info!("cp-av: screen: {e}");
                acc.truncate(have);
                break;
            }
        };
        acc.truncate(have + n);
        let mut off = 0;
        while acc.len() - off >= SCREEN_HEADER {
            let hdr = &acc[off..off + SCREEN_HEADER];
            let body_size = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
            if body_size > MAX_SCREEN_BODY {
                log::warn!("cp-av: screen body of {body_size} bytes, dropping the stream");
                break 'stream;
            }
            let body_size = body_size as usize;
            if acc.len() - off < SCREEN_HEADER + body_size {
                break;
            }
            let opcode = hdr[4];
            let (hdr, body) = acc[off..off + SCREEN_HEADER + body_size].split_at(SCREEN_HEADER);
            if opcode == OP_VIDEO_CONFIG {
                let (params, hevc) = nalu::config_to_annexb(body);
                if av.codec.swap(if hevc { 2 } else { 1 }, Ordering::AcqRel) == 0 {
                    let codec = if hevc { Codec::Hevc } else { Codec::H264 };
                    log::info!(
                        "cp-av: screen codec {codec}, {} bytes of parameter sets",
                        params.len()
                    );
                    s.report(Event::VideoCodec(codec));
                    s.set_phase(lehu_session::Phase::Ready);
                }
                *av.params.lock().unwrap_or_else(|e| e.into_inner()) = params.clone();
                if s.video_active.load(Ordering::Acquire) && !params.is_empty() {
                    let mut unit = take_unit();
                    if unit.write(&params) {
                        unit.config = true;
                        s.ports.video.push_drop_oldest(unit);
                    }
                }
                configs += 1;
            } else if opcode == OP_VIDEO_FRAME && body_size >= TAG_LEN {
                let ct_len = body_size - TAG_LEN;
                let (ct, tag) = body.split_at(ct_len);
                let mut unit = take_unit();
                let Some(dst) = unit.space(ct_len) else {
                    log::warn!("cp-av: a {ct_len}-byte frame does not fit the unit, dropped");
                    counter += 1;
                    dropped += 1;
                    off += SCREEN_HEADER + body_size;
                    continue;
                };
                // Decrypted straight inside the unit (the one copy on this
                // path), then reframed where it lies.
                dst.copy_from_slice(ct);
                if !aead.open_detached(&crypto::nonce64(counter), hdr, dst, tag) {
                    log::warn!(
                        "cp-av: frame {counter} failed to decrypt: the stream is unrecoverable"
                    );
                    break 'stream;
                }
                counter += 1;
                if !nalu::frame_to_annexb_in_place(dst) {
                    log::warn!("cp-av: a frame with a malformed NAL chain, dropped");
                    dropped += 1;
                    off += SCREEN_HEADER + body_size;
                    continue;
                }
                let hevc = av.hevc();
                let (params_only, keyframe) = {
                    let data = unit.data();
                    // Parameter sets inside the stream (a bitrate change)
                    // are kept too; a frame that is only parameter sets is
                    // rare, so the full scan runs only when its first NAL
                    // is one.
                    let first = data.get(4).copied().unwrap_or(0);
                    let first_is_params = if hevc {
                        (32..=34).contains(&((first >> 1) & 0x3f))
                    } else {
                        matches!(first & 0x1f, 7 | 8)
                    };
                    let params_only = first_is_params && !annexb::has_picture(data, hevc);
                    let keyframe = av.keyframe_pending.load(Ordering::Relaxed)
                        && annexb::kinds(data, hevc).keyframe;
                    (params_only, keyframe)
                };
                if params_only {
                    *av.params.lock().unwrap_or_else(|e| e.into_inner()) = unit.data().to_vec();
                    unit.config = true;
                }
                if keyframe {
                    av.keyframe_pending.store(false, Ordering::Relaxed);
                    unit.keyframe = true;
                }
                frames += 1;
                if frames == 1 {
                    log::info!("cp-av: first frame, {} bytes", unit.len());
                }
                if !s.video_active.load(Ordering::Acquire) {
                    // The screens are in front: the frame is not wanted,
                    // the unit goes back.
                    unit.clear();
                    s.ports.recycle.push_discard(unit);
                } else {
                    let pushed = s
                        .ports
                        .video
                        .push_wait_drop_oldest(unit, VIDEO_DEPTH, VIDEO_WAIT);
                    if av.recorded.load(Ordering::Acquire)
                        && !av.live_reported.swap(true, Ordering::AcqRel)
                    {
                        s.set_phase(lehu_session::Phase::Live);
                    }
                    // A frame the queue dropped is a gap: ask for a keyframe,
                    // which the decoder holds the picture for. One request
                    // per gap; a keyframe passing answers it, and it is
                    // asked again after a second in case it was lost.
                    if !pushed {
                        dropped += 1;
                        let now = now_ms() as u32;
                        if !av.keyframe_pending.load(Ordering::Relaxed)
                            || now.wrapping_sub(av.keyframe_asked.load(Ordering::Relaxed)) >= 1000
                        {
                            av.keyframe_pending.store(true, Ordering::Relaxed);
                            av.keyframe_asked.store(now, Ordering::Relaxed);
                            av.queue_command(info::force_keyframe_command(info::main_uuid(
                                &av.s.config,
                            )));
                        }
                    }
                }
            }
            off += SCREEN_HEADER + body_size;
        }
        if off > 0 {
            acc.drain(..off);
        }
        // A keyframe bigger than usual grew the buffer; once it has gone
        // through, the buffer goes back to its usual size.
        if acc.capacity() > 2 * ACC_BYTES && acc.len() < ACC_BYTES / 2 {
            acc.shrink_to(ACC_BYTES);
        }
    }
    log::info!(
        "cp-av: screen stream ended ({frames} frames, {configs} configs, {dropped} dropped)"
    );
}

// ── Audio down ────────────────────────────────────────────────────────────

/// RTP over UDP: `[12-byte header][ciphertext][16-byte tag][8-byte nonce]`,
/// sealed with the timestamp and SSRC (header bytes 4..12) as the
/// associated data. Decrypted in place and handed to the sink as it is.
#[allow(clippy::too_many_arguments)]
fn audio_loop(
    av: &Arc<Av>,
    data: &Arc<UdpSocket>,
    ctrl: &UdpSocket,
    stream_id: i64,
    stream_type: i64,
    kind: AudioKind,
    format: AudioFormat,
    uplink: bool,
    stop: &AtomicBool,
) {
    realtime(&av.s);
    let key = stream_key(&av.secret, stream_id, "DataStream-Output-Encryption-Key");
    let aead = ChaChaStream::new(&key);
    let mut buf = [0u8; 4096];
    let mut drain = [0u8; 512];
    let (mut packets, mut fails) = (0u64, 0u64);
    let mut last_packet = now_ms();
    let mut active = false;
    while av.running() && !stop.load(Ordering::Acquire) {
        while ctrl.recv_from(&mut drain).is_ok() {}
        let (n, from) = match data.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                // The call-mode safety net: the voice stream silent for 3 s
                // missed its TEARDOWN; Opus keeps refresh frames coming
                // every 400 ms, so a live call never trips it.
                if stream_type == info::STREAM_MAIN_AUDIO
                    && av.call_active.load(Ordering::Relaxed)
                    && av.call_stream_id() == stream_id
                    && now_ms().saturating_sub(last_packet) > CALL_SILENT_MS
                {
                    av.end_call("the voice stream fell silent");
                }
                continue;
            }
            Err(e) => {
                log::info!("cp-av: audio {stream_type}: {e}");
                break;
            }
        };
        if n < 12 + TAG_LEN + 8 {
            continue;
        }
        last_packet = now_ms();
        if !active {
            active = true;
            let mut metas = av.audio();
            if let Some(m) = metas.iter_mut().find(|m| m.stream_id == stream_id) {
                m.first_ms = Some(last_packet);
            }
            drop(metas);
            av.s.report(Event::Audio(kind, true));
            log::info!("cp-av: audio {stream_type} first packet ({} bytes)", n);
        }
        if uplink {
            let mut u = av.uplink();
            if let Some(u) = u.as_mut()
                && u.stream_id == stream_id
                && !u.peer_latched
                && let SocketAddr::V6(src) = from
            {
                // The seeded port stays; the address and scope are the
                // phone's own.
                let port = u.peer.port();
                u.peer = src;
                if u.peer_seeded {
                    u.peer.set_port(port);
                }
                u.peer_latched = true;
                u.payload_type = buf[1] & 0x7f;
                log::info!(
                    "cp-av: microphone goes to {} (PT {})",
                    u.peer,
                    u.payload_type
                );
            }
        }
        // Telephony priority: the media and alternate streams are drained
        // but not decrypted while a call runs.
        if stream_type != info::STREAM_MAIN_AUDIO && av.call_active.load(Ordering::Relaxed) {
            continue;
        }
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&buf[n - 8..n]);
        let (head, rest) = buf[..n - 8].split_at_mut(12);
        if !aead.open(&nonce, &head[4..12], rest) {
            if fails == 0 {
                log::warn!("cp-av: audio {stream_type} failed to decrypt");
            }
            fails += 1;
            continue;
        }
        let plain_len = rest.len() - TAG_LEN;
        let payload = &mut rest[..plain_len];
        if format.codec == AudioCodec::Pcm {
            // The wire is big-endian; the mixer takes native samples.
            for pair in payload.as_chunks_mut::<2>().0 {
                pair.swap(0, 1);
            }
        }
        packets += 1;
        av.s.sinks().audio_packet(kind, format, payload);
    }
    if active {
        av.s.report(Event::Audio(kind, false));
    }
    log::info!("cp-av: audio {stream_type} ended ({packets} packets, {fails} failed)");
}

// ── The microphone ────────────────────────────────────────────────────────

/// The mirror of the downlink framing, with the per-stream input key:
/// the loop's capture thread fills `ports.mic` with frames already coded
/// as the phone asked; each becomes one RTP packet.
fn uplink_loop(av: &Arc<Av>, stop: &AtomicBool) {
    realtime(&av.s);
    let mic = Arc::clone(&av.s.ports.mic);
    let mut pkt = [0u8; 12 + lehu_session::MicFrame::CAPACITY + TAG_LEN + 8];
    while av.running() && !stop.load(Ordering::Acquire) {
        let Some(frame) = mic.pop_wait(Duration::from_millis(100)) else {
            continue;
        };
        let mut guard = av.uplink();
        let Some(u) = guard.as_mut() else {
            continue;
        };
        // The frame is coded as the phone asked: CarPlay's LPCM is already
        // big-endian (the sink was made with `big_endian_pcm`).
        let len = usize::from(frame.len);
        pkt[0] = 0x80;
        pkt[1] = u.payload_type;
        pkt[2..4].copy_from_slice(&u.seq.to_be_bytes());
        pkt[4..8].copy_from_slice(&u.timestamp.to_be_bytes());
        pkt[8..12].copy_from_slice(&0u32.to_be_bytes());
        pkt[12..12 + len].copy_from_slice(&frame.data[..len]);
        let nonce = crypto::nonce64(u.nonce);
        let (head, body) = pkt.split_at_mut(12);
        let Some(tag) = u.aead.seal(&nonce, &head[4..12], &mut body[..len]) else {
            continue;
        };
        pkt[12 + len..12 + len + TAG_LEN].copy_from_slice(&tag);
        pkt[12 + len + TAG_LEN..12 + len + TAG_LEN + 8].copy_from_slice(&u.nonce.to_le_bytes());
        let total = 12 + len + TAG_LEN + 8;
        match u.socket.send_to(&pkt[..total], SocketAddr::V6(u.peer)) {
            Ok(_) => u.window_sent += 1,
            Err(e) => {
                if u.window_failed == 0 {
                    log::warn!("cp-av: a microphone frame was not sent: {e}");
                }
                u.window_failed += 1;
            }
        }
        let now = now_ms();
        if now >= u.report_at {
            log::info!(
                "[Mic] {} frames to the phone, {} not sent ({:?}, {} samples each)",
                u.window_sent,
                u.window_failed,
                u.codec,
                u.frame_samples
            );
            u.window_sent = 0;
            u.window_failed = 0;
            u.report_at = now + UPLINK_REPORT_MS;
        }
        u.seq = u.seq.wrapping_add(1);
        u.timestamp = u.timestamp.wrapping_add(u.frame_samples);
        u.nonce += 1;
        u.sent += 1;
        if u.sent == 1 {
            log::info!(
                "cp-av: first microphone frame to the phone (stream {})",
                u.stream_id as u64
            );
        }
    }
}

// ── Timing ────────────────────────────────────────────────────────────────

/// The RTCP-style clock sync: the phone's requests answered with our
/// time in its domain, and ours sent every second to steer our clock onto
/// the phone's. A clock far off the phone's ends the session, so we
/// follow it. Its answers are what the silence watch listens for.
fn timing_loop(av: &Arc<Av>, socket: &UdpSocket, phone_port: u16, stop: &AtomicBool) {
    let mut dst = av.peer;
    dst.set_port(phone_port);
    let mut pending_t1 = 0u64;
    let mut last_send = Instant::now() - Duration::from_secs(2);
    let mut msg = [0u8; 64];
    while av.running() && !stop.load(Ordering::Acquire) {
        if last_send.elapsed() >= Duration::from_secs(1) && phone_port != 0 {
            let mut pkt = [0u8; 32];
            pkt[0] = 0x80;
            pkt[1] = PT_REQUEST;
            pkt[3] = 7;
            pending_t1 = av.timing.synced();
            pkt[24..32].copy_from_slice(&pending_t1.to_be_bytes());
            let _ = socket.send_to(&pkt, SocketAddr::V6(dst));
            last_send = Instant::now();
        }
        let Ok((n, from)) = socket.recv_from(&mut msg) else {
            continue;
        };
        if n < 32 {
            continue;
        }
        if msg[1] == PT_REQUEST {
            let mut resp = [0u8; 32];
            resp[0] = 0x80;
            resp[1] = PT_RESPONSE;
            resp[3] = 7;
            resp[8..16].copy_from_slice(&msg[24..32]);
            let now = av.timing.synced().to_be_bytes();
            resp[16..24].copy_from_slice(&now);
            resp[24..32].copy_from_slice(&now);
            let _ = socket.send_to(&resp, from);
        } else if msg[1] == PT_RESPONSE {
            let t1 = u64::from_be_bytes(msg[8..16].try_into().unwrap_or([0; 8]));
            let t2 = u64::from_be_bytes(msg[16..24].try_into().unwrap_or([0; 8]));
            let t3 = u64::from_be_bytes(msg[24..32].try_into().unwrap_or([0; 8]));
            let t4 = av.timing.synced();
            if t1 == pending_t1 && t1 != 0 {
                let offset = (t2.wrapping_sub(t1) as i64 + t3.wrapping_sub(t4) as i64) / 2;
                av.timing.steer(offset);
                pending_t1 = 0;
            }
        }
    }
}

// ── The iAP tunnel ────────────────────────────────────────────────────────

/// The phone carries iAP2 on inside the session (stream 130): the same
/// sealed framing as the control channel under the data-stream keys, then
/// 32-byte packages. Received and logged; nothing is answered on it yet.
fn tunnel_loop(av: &Arc<Av>, mut stream: TcpStream, seed: i64, stop: &AtomicBool) {
    let read_key = stream_key(&av.secret, seed, "DataStream-Output-Encryption-Key");
    let write_key = stream_key(&av.secret, seed, "DataStream-Input-Encryption-Key");
    let mut cipher = FrameCipher::new(&read_key, &write_key);
    let _ = stream.set_read_timeout(Some(POLL));
    let mut sealed = Vec::new();
    let mut plain = Vec::new();
    let mut buf = [0u8; 16384];
    while av.running() && !stop.load(Ordering::Acquire) {
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => break,
        };
        sealed.extend_from_slice(&buf[..n]);
        if !cipher.decrypt(&mut sealed, &mut plain) {
            log::warn!("cp-av: iAP tunnel decrypt failed");
            break;
        }
        let mut off = 0;
        while plain.len() - off >= 32 {
            let size =
                u32::from_be_bytes([plain[off], plain[off + 1], plain[off + 2], plain[off + 3]])
                    as usize;
            if !(32..=4 * 1024 * 1024).contains(&size) {
                log::warn!("cp-av: iAP tunnel package of {size} bytes");
                return;
            }
            if plain.len() - off < size {
                break;
            }
            let kind = u32::from_be_bytes([
                plain[off + 16],
                plain[off + 17],
                plain[off + 18],
                plain[off + 19],
            ]);
            log::debug!("cp-av: iAP tunnel package {kind:#010x}, {size} bytes");
            off += size;
        }
        plain.drain(..off);
    }
    log::info!("cp-av: iAP tunnel closed");
}

#[cfg(test)]
mod tests {
    use super::*;

    use lehu_core::poll::Wake;
    use lehu_session::{AudioFormat, AudioKind, MicFormat, Ports, Sinks};

    use crate::Config;

    /// What the session asked of the microphone.
    struct MicLog(Arc<Mutex<Vec<bool>>>);

    impl Sinks for MicLog {
        fn audio_pcm(&mut self, _: AudioKind, _: u32, _: u8, _: &[i16]) {}
        fn audio_packet(&mut self, _: AudioKind, _: AudioFormat, _: &[u8]) {}
        fn mic(&mut self, wanted: Option<MicFormat>) {
            self.0.lock().unwrap().push(wanted.is_some());
        }
    }

    #[test]
    fn a_teardown_by_type_ends_the_call_and_its_microphone_and_keeps_the_streams() {
        let mic = Arc::new(Mutex::new(Vec::new()));
        let wake = Arc::new(Wake::new().unwrap());
        let ports = Ports::new(wake, Arc::new(Queue::new(8)), Arc::new(Queue::new(8)));
        let shared = Shared::for_test(
            Config::for_test(Codec::H264),
            Box::new(MicLog(Arc::clone(&mic))),
            ports,
        );
        let peer = SocketAddrV6::new(Ipv6Addr::LOCALHOST, 1, 0, 0);
        let mut session = AvSession::new(shared, ConnHandle::new(), [0; 32], peer);
        let av = Arc::clone(&session.av);
        // Media from before the call, the call (with the microphone), and
        // a stream of another type.
        for (stream_type, stream_id) in [(100, 5), (100, 6), (101, 7)] {
            av.audio().push(AudioMeta {
                stream_type,
                stream_id,
                rate: 48000,
                first_ms: None,
            });
        }
        av.set_call_stream(6);
        av.call_active.store(true, Ordering::Release);
        *av.uplink() = Some(Uplink {
            stream_id: 6,
            socket: Arc::new(UdpSocket::bind("[::1]:0").unwrap()),
            peer,
            peer_seeded: true,
            peer_latched: true,
            payload_type: 100,
            aead: ChaChaStream::new(&[0; 32]),
            codec: AudioCodec::Pcm,
            frame_samples: 360,
            seq: 0,
            timestamp: 0,
            nonce: 0,
            sent: 0,
            window_sent: 0,
            window_failed: 0,
            report_at: 0,
        });

        // What a wired phone sends when a call ends (and when music
        // pauses): the type, no id.
        let mut stream = Value::dict();
        stream.set("type", Value::Int(100));
        let mut body = Value::dict();
        body.set("streams", Value::Array(vec![stream]));
        let req = Request {
            method: "TEARDOWN".into(),
            path: "/".into(),
            protocol: "RTSP/1.0".into(),
            headers: Vec::new(),
            body: plist::encode(&body),
        };
        session.handle_teardown(&req);

        // The streams stay: the phone goes on sending on them.
        let left: Vec<i64> = av.audio().iter().map(|m| m.stream_id).collect();
        assert_eq!(left, vec![5, 6, 7]);
        assert!(!av.call_active.load(Ordering::Acquire));
        assert!(av.uplink().is_none());
        assert_eq!(*mic.lock().unwrap(), vec![false]);
    }

    #[test]
    fn stream_keys_render_the_id_unsigned() {
        let secret = [3u8; 32];
        let negative = stream_key(&secret, -1, "DataStream-Output-Encryption-Key");
        let unsigned = crypto::hkdf_sha512(
            &secret,
            "DataStream-Salt18446744073709551615",
            "DataStream-Output-Encryption-Key",
        );
        assert_eq!(negative, unsigned);
        let signed = crypto::hkdf_sha512(
            &secret,
            "DataStream-Salt-1",
            "DataStream-Output-Encryption-Key",
        );
        assert_ne!(negative, signed);
        assert_eq!(
            stream_key(&secret, 7, "x"),
            crypto::hkdf_sha512(&secret, "DataStream-Salt7", "x")
        );
    }

    #[test]
    fn formats_and_kinds() {
        assert_eq!(
            audio_format_of(0x10),
            AudioFormat {
                codec: AudioCodec::Pcm,
                rate: 16000,
                channels: 1
            }
        );
        assert_eq!(audio_format_of(0x8).channels, 2);
        assert_eq!(audio_format_of(0x8).rate, 8000);
        assert_eq!(audio_format_of(0x80_0000).codec, AudioCodec::AacLc);
        assert_eq!(audio_format_of(0x80_0000).rate, 48000);
        assert_eq!(audio_format_of(0x2000_0000).codec, AudioCodec::Opus);
        assert_eq!(audio_kind_of(102, "media"), AudioKind::Media);
        assert_eq!(audio_kind_of(100, "media"), AudioKind::Media);
        assert_eq!(audio_kind_of(100, "telephony"), AudioKind::Call);
        assert_eq!(audio_kind_of(100, "alert"), AudioKind::System);
        assert_eq!(audio_kind_of(101, "default"), AudioKind::Speech);
        assert_eq!(audio_kind_of(100, "speechRecognition"), AudioKind::Speech);
    }
}
