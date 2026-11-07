//! The playback thread: one thread owns the PCM and mixes the two queues
//! (main = buffered media, aux = voice and prompts) in software, once per
//! period.
//!
//! A blocking write sleeps on the codec's clock, so the thread wakes
//! exactly once per period and idles at nothing; with no device open it
//! waits on the voice queue (voice onset is what must be quick; music
//! tolerates the 100 ms poll). FastCarPlay measured the alternative, two
//! queued-audio devices summed by a plugin, at most of the core during a
//! call, while the mixing itself is trivial integer work.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use lehu_core::clock;
use lehu_core::queue::Queue;
use lehu_core::settings::Audio;
use lehu_session::AudioKind;

use crate::alsa::{self, Pcm};
use crate::engine::{Engine, MAX_BUFFER_MS, MIX_BUFFER, MIX_CHANNELS, MIX_PERIOD, Tuning};
use crate::priority;
use crate::segment::Segment;
use crate::source::MIX_RATE;
use crate::volume::Volume;

/// Which of the mixer's two inputs a stream goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// Music: buffered, never capped.
    Main,
    /// Voice, prompts, chimes, the call: low latency, ducks the music.
    Aux,
}

impl Input {
    pub fn of(kind: AudioKind) -> Input {
        match kind {
            AudioKind::Media => Input::Main,
            AudioKind::Speech | AudioKind::System | AudioKind::Call => Input::Aux,
        }
    }
}

/// The depth of the main queue: about 2.7 s of CarPlay's 21 ms media
/// segments, room for a burst on top of the deepest buffer.
pub const MAIN_DEPTH: usize = 128;
/// The depth of the aux queue: a few hundred ms of voice.
pub const AUX_DEPTH: usize = 24;

/// How long the idle thread sleeps on the voice queue between looks at
/// both queues, and how long after a failed open before the next try.
const IDLE_WAIT: Duration = Duration::from_millis(100);
const RETRY_WAIT: Duration = Duration::from_millis(500);

pub struct Mixer {
    active: Arc<AtomicBool>,
    buffer_ms: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl Mixer {
    /// Start the thread on `device`. Segments it consumes are freed.
    pub fn start(
        device: &str,
        settings: &Audio,
        main: Arc<Queue<Segment>>,
        aux: Arc<Queue<Segment>>,
        volume: Arc<Volume>,
    ) -> Mixer {
        Self::start_recycling(device, settings, main, aux, None, volume)
    }

    /// As `start`, with consumed segments going back, cleared, into
    /// `recycle` for the producer to fill again (freed when it is full).
    pub fn start_recycling(
        device: &str,
        settings: &Audio,
        main: Arc<Queue<Segment>>,
        aux: Arc<Queue<Segment>>,
        recycle: Option<Arc<Queue<Segment>>>,
        volume: Arc<Volume>,
    ) -> Mixer {
        let active = Arc::new(AtomicBool::new(true));
        let buffer_ms = Arc::new(AtomicU32::new(settings.buffer_ms.min(MAX_BUFFER_MS)));
        // The one floating-point step, once: the setting is a fraction.
        let fade_floor_q15 = ((settings.fade.clamp(0.0, 1.0) * 32768.0) + 0.5) as i32;
        let tuning = Tuning {
            fade_floor_q15,
            call_prefill_segments: settings.call_prefill_segments as usize,
            voice_max_latency_ms: settings.voice_max_latency_ms,
            idle_hold_ms: settings.idle_hold_ms,
        };
        let mut worker = Worker {
            device: device.to_string(),
            active: active.clone(),
            buffer_ms: buffer_ms.clone(),
            aux_q: aux.clone(),
            engine: Engine::new(main, aux, recycle, tuning),
            volume,
            pcm: None,
            out: vec![0; MIX_PERIOD * MIX_CHANNELS],
            open_failed: false,
        };
        let thread = std::thread::Builder::new()
            .name("audio-mixer".into())
            .spawn(move || worker.run())
            .ok();
        if thread.is_none() {
            log::error!("mixer: cannot start the thread");
            active.store(false, Ordering::Release);
        }
        Mixer {
            active,
            buffer_ms,
            thread,
        }
    }

    /// The music buffer, live: the thread follows it at the next period.
    pub fn set_buffer_ms(&self, ms: u32) {
        self.buffer_ms
            .store(ms.min(MAX_BUFFER_MS), Ordering::Relaxed);
    }

    /// Stop the thread and release the device. Returns once joined, within
    /// about a wait plus a period.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.active.store(false, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Mixer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Worker {
    device: String,
    active: Arc<AtomicBool>,
    buffer_ms: Arc<AtomicU32>,
    aux_q: Arc<Queue<Segment>>,
    engine: Engine,
    volume: Arc<Volume>,
    pcm: Option<Pcm>,
    out: Vec<i16>,
    open_failed: bool,
}

impl Worker {
    fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Wait for voice, or `wait`; a segment that arrives is kept for the
    /// aux source.
    fn idle_wait(&mut self, wait: Duration) {
        if let Some(seg) = self.aux_q.pop_wait(wait) {
            self.engine.aux.pend(seg);
        }
    }

    fn open(&mut self) -> bool {
        match Pcm::open_playback(
            &self.device,
            MIX_RATE,
            MIX_CHANNELS as u32,
            MIX_PERIOD,
            MIX_BUFFER,
        ) {
            Ok((pcm, got)) => {
                log::info!(
                    "mixer: {} open: {} Hz {}ch S16, period {}, buffer {}",
                    self.device,
                    got.rate,
                    got.channels,
                    got.period,
                    got.buffer
                );
                self.pcm = Some(pcm);
                self.open_failed = false;
                // A card that was unplugged and came back gets its volume
                // again, from the loop's next tick.
                self.engine.on_open(self.volume.software_gain_q15());
                self.volume.output_opened();
                true
            }
            Err(e) => {
                if !self.open_failed {
                    log::warn!("mixer: open {} failed: {e}", self.device);
                }
                self.open_failed = true;
                false
            }
        }
    }

    fn close(&mut self, why: &str) {
        if self.pcm.take().is_some() {
            log::debug!("mixer: device released ({why})");
        }
        self.engine.on_close();
    }

    fn run(&mut self) {
        priority::audio_thread("mixer");
        log::debug!("mixer: thread started");
        while self.active() {
            if self.pcm.is_none() {
                // Idle: no device. Silence a phone keeps sending while
                // nothing plays is dropped here, so the codec (the DAC and
                // the headphone amplifier, whose idle hiss is audible)
                // stays powered down.
                if !self.engine.drop_silence() {
                    self.idle_wait(IDLE_WAIT);
                    continue;
                }
                if !self.open() {
                    self.idle_wait(RETRY_WAIT);
                    continue;
                }
            }
            let now = clock::now_ms();
            if let Some(line) = self.engine.report(now) {
                log::info!("{line}");
            }
            let buffer = self.buffer_ms.load(Ordering::Relaxed);
            let gain = self.volume.software_gain_q15();
            let period = self.engine.period(now, buffer, gain, &mut self.out);

            let Some(pcm) = &self.pcm else {
                continue;
            };
            if let Err(err) = pcm.write(&self.out, MIX_PERIOD) {
                if err == -libc::EPIPE {
                    self.engine.underrun();
                }
                if let Err(e) = pcm.recover(err) {
                    log::warn!("mixer: write failed: {}; reopening", alsa::strerror(e));
                    self.close("write failed");
                    continue;
                }
                // Once more after the recovery; a second failure is the
                // next period's.
                let _ = pcm.write(&self.out, MIX_PERIOD);
            }
            if period.release {
                self.close("idle");
            }
        }
        self.close("stopped");
        log::debug!("mixer: thread stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_route_to_the_two_inputs() {
        assert_eq!(Input::of(AudioKind::Media), Input::Main);
        assert_eq!(Input::of(AudioKind::Speech), Input::Aux);
        assert_eq!(Input::of(AudioKind::System), Input::Aux);
        assert_eq!(Input::of(AudioKind::Call), Input::Aux);
    }

    #[test]
    fn starts_and_stops_without_a_device() {
        let settings = Audio::default();
        let volume = Arc::new(Volume::open(&Audio {
            volume_control: "software".into(),
            ..Audio::default()
        }));
        let main = Arc::new(Queue::new(MAIN_DEPTH));
        let aux = Arc::new(Queue::new(AUX_DEPTH));
        let m = Mixer::start(
            "lehu-test-no-such-device",
            &settings,
            main.clone(),
            aux.clone(),
            volume,
        );
        m.set_buffer_ms(9000);
        assert_eq!(m.buffer_ms.load(Ordering::Relaxed), MAX_BUFFER_MS);
        // Silence queued while idle is dropped without a device.
        main.push_discard(Segment::from_slice(
            crate::segment::Format::STEREO_48K,
            &[0; 64],
        ));
        std::thread::sleep(Duration::from_millis(250));
        assert!(main.is_empty());
        m.stop();
    }
}
