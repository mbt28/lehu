//! One phone session as the main loop sees it, whichever protocol speaks it.
//!
//! A protocol crate (CarPlay, Android Auto) runs its link on its own threads
//! and talks to the loop through three things defined here: [`Event`]s it
//! pushes into a bounded queue (and a [`Wake`] of the loop), [`Cmd`]s the
//! loop hands it, and [`Sinks`] for the media it receives. The loop keeps one
//! `Box<dyn Session>` at a time; the protocol never touches the screen, the
//! decoder or the mixer itself. Video goes straight into the decoder's queue
//! as Annex-B access units, since that path is the hot one.
//!
//! Everything here is `Copy` and fixed-size so the queues never allocate;
//! what a phone tells in words (now playing, route guidance) goes through
//! the side channel in [`side`], named by an event.

use std::sync::Arc;

use lehu_core::poll::Wake;
use lehu_core::queue::Queue;
pub use lehu_core::settings::NightMode;
pub use lehu_input::{Contact, MAX_SLOTS, PhoneButton, TOUCH_SCALE};
pub use lehu_video::{AccessUnit, Codec};
pub use side::{Artwork, Guidance, Latest, MAX_ARTWORK, Maneuver, NowPlaying, Playback, set_text};

mod side;

/// Where a session is in its life. Ordered: a session only moves forward,
/// except Live <-> Backgrounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// The link is up, nothing known about the phone yet.
    Spawned,
    /// The phone said who it is (its name, its id).
    Identified,
    /// Streams are negotiated; the video codec is known.
    Ready,
    /// Video is flowing and the phone holds the screen.
    Live,
    /// The session runs but the head unit's screens are in front.
    Backgrounded,
    /// Goodbye sent; waiting for the phone or the timeout.
    Closing,
    /// Over. The loop drops the session on seeing this.
    Closed,
}

impl Phase {
    /// Whether the phone's video may be on the panel.
    pub fn shows_video(self) -> bool {
        self == Phase::Live
    }

    /// Whether the session still exists for the loop's purposes.
    pub fn alive(self) -> bool {
        self < Phase::Closed
    }
}

/// How the phone is attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Usb,
    Wifi,
}

/// Which protocol runs the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    CarPlay,
    AndroidAuto,
}

/// The audio streams a phone sends, by what the mixer does with them: media
/// is the music source, the other three are the voice source that ducks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioKind {
    /// Music, podcasts: buffered, never capped.
    Media,
    /// Navigation prompts and the assistant's voice: low latency.
    Speech,
    /// Chimes and alerts.
    System,
    /// The call's downlink; the microphone runs while it does.
    Call,
}

/// How a stream's bytes are coded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCodec {
    /// Signed 16-bit little-endian, interleaved.
    Pcm,
    AacLc,
    Opus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub codec: AudioCodec,
    pub rate: u32,
    pub channels: u8,
}

/// What the phone wants from the microphone: the uplink's coding and
/// framing, so the capture thread produces frames the protocol sends as
/// they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MicFormat {
    pub codec: AudioCodec,
    pub rate: u32,
    /// Frame length in tenths of a millisecond: 75 for CarPlay's wired
    /// 7.5 ms LPCM frames, 200 for 20 ms Opus and Android Auto.
    pub frame_tenths_ms: u16,
}

/// One microphone frame, encoded as the protocol wants it. 20 ms of 16 kHz
/// mono PCM is 640 bytes; Opus frames are smaller.
#[derive(Clone, Copy, Debug)]
pub struct MicFrame {
    pub len: u16,
    pub data: [u8; MicFrame::CAPACITY],
}

impl MicFrame {
    pub const CAPACITY: usize = 1024;

    pub fn empty() -> Self {
        Self {
            len: 0,
            data: [0; Self::CAPACITY],
        }
    }

    /// A frame holding `bytes`, or `None` when it does not fit.
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > Self::CAPACITY {
            return None;
        }
        let mut f = Self::empty();
        f.data[..bytes.len()].copy_from_slice(bytes);
        f.len = bytes.len() as u16;
        Some(f)
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data[..usize::from(self.len)]
    }
}

/// Why a session ended or stumbled, for the status line and the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The link dropped under the session.
    LinkLost,
    /// Nothing heard from the phone for the protocol's silence limit.
    Silent,
    /// The phone refused a step of the handshake or pairing.
    Rejected,
    /// A message the phone sent made no sense; the session goes on.
    BadMessage,
    /// The session's own setup timed out.
    SetupTimeout,
    /// The phone said goodbye.
    PhoneLeft,
}

/// What a session tells the loop. Pushed into the events queue with
/// `push_drop_oldest` and followed by a wake; the loop drains the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Phase(Phase),
    /// The video stream's codec; the loop starts the decoder on it. Comes
    /// before `Phase(Ready)`.
    VideoCodec(Codec),
    /// The phone wants the screen (true) or hands it back (false).
    VideoFocus(bool),
    /// An audio stream of this kind started or stopped.
    Audio(AudioKind, bool),
    /// The phone dropped the media it had sent ahead (a pause, a skip):
    /// what is buffered of it must not play.
    AudioFlush,
    /// The phone's navigation is guiding or stopped.
    NavFocus(bool),
    Call(bool),
    /// Siri or the Assistant is listening.
    Assistant(bool),
    /// The phone asks for the head unit's own UI (its Home or Exit button).
    HostUi,
    /// The phone's wall clock, UTC milliseconds since the Unix epoch, and
    /// its time zone's offsets in minutes (CarPlay's DeviceTimeUpdate): a
    /// head unit has no clock of its own worth the name.
    Clock {
        unix_ms: i64,
        zone_offset_min: i16,
        dst_offset_min: i8,
    },
    /// What the phone plays changed: `Ports::now_playing` has it.
    NowPlaying,
    /// The route guidance changed: `Ports::guidance` has it.
    Guidance,
    Fault(Fault),
}

/// What the loop tells a session. Handled on the session's threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    /// One frame of touch contacts, in `TOUCH_SCALE` units of the panel; the
    /// session maps them to the phone's resolution.
    Touch {
        count: u8,
        contacts: [Contact; MAX_SLOTS],
    },
    Key {
        button: PhoneButton,
        down: bool,
    },
    /// Knob detents, positive clockwise.
    Rotary(i8),
    /// Ask for a keyframe (and replay the parameter sets first).
    Keyframe,
    /// Whether the decoder shows the phone's video: the session tells the
    /// phone it has the screen or lost it.
    VideoActive(bool),
    Night(NightMode),
    /// Bring the phone's video back after the screens.
    Resume,
    /// Say goodbye and end.
    Close,
}

/// Where a session puts the media it receives. Implemented by the loop,
/// called on the session's threads: every method copies into a bounded
/// queue and returns at once.
pub trait Sinks: Send {
    /// Decoded samples, as Android Auto sends them.
    fn audio_pcm(&mut self, kind: AudioKind, rate: u32, channels: u8, pcm: &[i16]);
    /// A still-encoded packet, as CarPlay sends them; the mixer decodes.
    fn audio_packet(&mut self, kind: AudioKind, format: AudioFormat, payload: &[u8]);
    /// The phone wants the microphone in this format, or no longer.
    fn mic(&mut self, wanted: Option<MicFormat>);
}

/// The loop's ends of a session's pipes, handed to `start`.
#[derive(Clone)]
pub struct Ports {
    pub events: Arc<Queue<Event>>,
    pub wake: Arc<Wake>,
    /// Annex-B access units for the decoder: `push_wait_drop_oldest` with a
    /// depth of 4 and 500 ms, the units taken from `recycle` when it has one.
    pub video: Arc<Queue<AccessUnit>>,
    pub recycle: Arc<Queue<AccessUnit>>,
    /// Microphone frames the capture thread fills while `Sinks::mic` is on.
    pub mic: Arc<Queue<MicFrame>>,
    /// What the phone plays and its route guidance, in words; written by
    /// the session, read by the loop on `Event::NowPlaying` and
    /// `Event::Guidance`.
    pub now_playing: Arc<Latest<NowPlaying>>,
    pub guidance: Arc<Latest<Guidance>>,
    /// The album art of what plays, compressed as the phone sent it; its
    /// changes are reported as `Event::NowPlaying` too.
    pub artwork: Arc<Latest<Artwork>>,
}

/// The depth of the events queue: a session reports a handful of changes a
/// second at most; over this the loop has stalled.
pub const EVENTS_DEPTH: usize = 32;

/// The depth of the microphone queue: 100 ms of 7.5 ms frames.
pub const MIC_DEPTH: usize = 16;

impl Ports {
    pub fn new(
        wake: Arc<Wake>,
        video: Arc<Queue<AccessUnit>>,
        recycle: Arc<Queue<AccessUnit>>,
    ) -> Self {
        Self {
            events: Arc::new(Queue::new(EVENTS_DEPTH)),
            wake,
            video,
            recycle,
            mic: Arc::new(Queue::new(MIC_DEPTH)),
            now_playing: Arc::new(Latest::new()),
            guidance: Arc::new(Latest::new()),
            artwork: Arc::new(Latest::new()),
        }
    }

    /// Report an event and wake the loop. Only the oldest event is lost when
    /// the loop fell far behind, never the newest phase.
    pub fn report(&self, event: Event) {
        self.events.push_drop_oldest(event);
        self.wake.wake();
    }

    /// Change what the phone plays; `f` says whether it did, and only then
    /// is the loop told.
    pub fn tell_now_playing(&self, f: impl FnOnce(&mut NowPlaying) -> bool) -> bool {
        let changed = self.now_playing.update(f);
        if changed {
            self.report(Event::NowPlaying);
        }
        changed
    }

    /// Change the album art, as `tell_now_playing`; the event is the
    /// same, since the art is part of what plays.
    pub fn tell_artwork(&self, f: impl FnOnce(&mut Artwork) -> bool) -> bool {
        let changed = self.artwork.update(f);
        if changed {
            self.report(Event::NowPlaying);
        }
        changed
    }

    /// Change the route guidance, as `tell_now_playing`.
    pub fn tell_guidance(&self, f: impl FnOnce(&mut Guidance) -> bool) -> bool {
        let changed = self.guidance.update(f);
        if changed {
            self.report(Event::Guidance);
        }
        changed
    }
}

/// A running session. Dropping it without `stop` is the abrupt end: the
/// threads are told to quit and joined, nothing is sent to the phone.
pub trait Session: Send {
    fn kind(&self) -> Kind;
    fn link(&self) -> Link;
    fn phase(&self) -> Phase;
    /// The phone's name once `Identified`, for the status line.
    fn phone(&self) -> Option<&str>;
    /// The protocol's lasting id for the phone once `Identified` (CarPlay's
    /// pairing id, Android Auto's device id), the key of the device
    /// registry; `None` until known.
    fn identity(&self) -> Option<String> {
        None
    }
    /// The Bluetooth address of a wireless session's phone ("AA:BB:..."),
    /// the one whose handoff led it here; `None` over a cable.
    fn bt_address(&self) -> Option<String> {
        None
    }
    /// The session runs CarPlay Ultra rather than classic CarPlay.
    fn ultra(&self) -> bool {
        false
    }
    fn command(&self, cmd: Cmd);
    /// The graceful end: goodbye, then join, bounded by the protocol's wait.
    fn stop(self: Box<Self>);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_order_forward() {
        assert!(Phase::Spawned < Phase::Identified);
        assert!(Phase::Identified < Phase::Ready);
        assert!(Phase::Ready < Phase::Live);
        assert!(Phase::Live < Phase::Backgrounded);
        assert!(Phase::Backgrounded < Phase::Closing);
        assert!(Phase::Closing < Phase::Closed);
        assert!(Phase::Closing.alive());
        assert!(!Phase::Closed.alive());
        assert!(Phase::Live.shows_video());
        assert!(!Phase::Backgrounded.shows_video());
    }

    #[test]
    fn mic_frame_bounds() {
        let f = MicFrame::from_slice(&[7; 640]).expect("fits");
        assert_eq!(f.as_slice().len(), 640);
        assert!(MicFrame::from_slice(&[0; MicFrame::CAPACITY + 1]).is_none());
        assert!(MicFrame::empty().as_slice().is_empty());
    }

    #[test]
    fn report_wakes_and_keeps_newest() {
        let wake = Arc::new(Wake::new().expect("eventfd"));
        let units = Arc::new(Queue::new(1));
        let ports = Ports::new(wake.clone(), units.clone(), units);
        for i in 0..(EVENTS_DEPTH + 3) {
            let on = i % 2 == 0;
            ports.report(Event::VideoFocus(on));
        }
        ports.report(Event::Phase(Phase::Closed));
        assert!(wake.drain() > 0);
        let mut last = None;
        while let Some(e) = ports.events.pop() {
            last = Some(e);
        }
        assert_eq!(last, Some(Event::Phase(Phase::Closed)));
        assert!(ports.events.drops() >= 4);
    }

    #[test]
    fn the_side_channel_is_named_by_an_event() {
        let wake = Arc::new(Wake::new().expect("eventfd"));
        let units = Arc::new(Queue::new(1));
        let ports = Ports::new(wake.clone(), units.clone(), units);
        assert!(!ports.tell_guidance(|_| false));
        assert!(ports.events.pop().is_none(), "no change, no event");
        assert!(ports.tell_guidance(|g| {
            g.active = true;
            true
        }));
        assert_eq!(ports.events.pop(), Some(Event::Guidance));
        assert!(wake.drain() > 0);
        let mut g = Guidance::default();
        assert!(ports.guidance.read_into(0, &mut g).is_some());
        assert!(g.active);
        assert!(ports.tell_now_playing(|_| true));
        assert_eq!(ports.events.pop(), Some(Event::NowPlaying));
    }
}
