// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The program's audio: one mixer and one volume for its lifetime, and a
//! sink per phone session that turns what the protocol receives into the
//! mixer's segments and runs the microphone when the phone asks.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lehu_audio::{AUX_DEPTH, Decoder, Input, MAIN_DEPTH, Mic, Mixer, Segment, Volume};
use lehu_core::queue::Queue;
use lehu_core::settings::Audio as AudioSettings;
use lehu_session::{AudioFormat, AudioKind, MicFormat, MicFrame, Sinks};

/// A segment never holds more than this: CarPlay's media packets are 1024
/// frames of stereo, Android Auto's PCM chunks a few thousand samples.
const SEGMENT_SAMPLES: usize = 8192;

/// The audio path for the program's lifetime, shared by the screens (the
/// volume rows) and the session driver (the sinks). The mixer's thread
/// idles with the device closed between sessions, so there is no start
/// and stop per session.
pub struct Audio {
    pub volume: Arc<Volume>,
    mixer: Mutex<Option<Mixer>>,
    /// The output the mixer plays on.
    device: Mutex<String>,
    main: Arc<Queue<Segment>>,
    aux: Arc<Queue<Segment>>,
    /// The microphone's PCM, read each time a microphone starts.
    mic_device: Arc<Mutex<String>>,
    mic_gain: u32,
    /// Each stream's level in Q15, by `slot` (music, navigation, alerts,
    /// calls); read by the sessions' sinks, set live from the screens.
    levels: Arc<[AtomicI32; 4]>,
}

/// The microphone's PCM: its own setting, or the output's card.
fn mic_device_of(settings: &AudioSettings) -> String {
    if settings.mic_device.is_empty() {
        settings.device.clone()
    } else {
        settings.mic_device.clone()
    }
}

/// A level in percent as Q15: 100 is 32768, unchanged.
fn q15_of(percent: u32) -> i32 {
    (percent.min(100) * 32768 / 100) as i32
}

impl Audio {
    pub fn start(settings: &AudioSettings) -> Self {
        let volume = Arc::new(Volume::open(settings));
        let main = Arc::new(Queue::new(MAIN_DEPTH));
        let aux = Arc::new(Queue::new(AUX_DEPTH));
        let mixer = Mixer::start(
            &settings.device,
            settings,
            Arc::clone(&main),
            Arc::clone(&aux),
            Arc::clone(&volume),
        );
        Self {
            volume,
            mixer: Mutex::new(Some(mixer)),
            device: Mutex::new(settings.device.clone()),
            main,
            aux,
            mic_device: Arc::new(Mutex::new(mic_device_of(settings))),
            mic_gain: settings.mic_gain,
            levels: Arc::new(std::array::from_fn(|_| AtomicI32::new(32768))),
        }
        .with_levels(settings)
    }

    fn with_levels(self, settings: &AudioSettings) -> Self {
        self.set_levels(settings);
        self
    }

    /// The streams' levels from the settings, live: segments queued
    /// already play at the level they were given.
    pub fn set_levels(&self, s: &AudioSettings) {
        let levels = [
            s.music_level,
            s.navigation_level,
            s.alert_level,
            s.call_level,
        ];
        for (slot, percent) in self.levels.iter().zip(levels) {
            slot.store(q15_of(percent), Ordering::Relaxed);
        }
    }

    /// The output and the microphone, live. A new output restarts the
    /// mixer on it (what is queued waits in the queues, which stay) and
    /// moves the volume control to its card; a new microphone is opened
    /// the next time one starts.
    pub fn set_devices(&self, settings: &AudioSettings) {
        *self
            .mic_device
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = mic_device_of(settings);
        let mut device = self.device.lock().unwrap_or_else(PoisonError::into_inner);
        if *device == settings.device {
            return;
        }
        log::info!("audio: output {} from now on", settings.device);
        device.clone_from(&settings.device);
        let mut mixer = self.mixer.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(m) = mixer.take() {
            m.stop();
        }
        self.volume.rebind(settings);
        *mixer = Some(Mixer::start(
            &settings.device,
            settings,
            Arc::clone(&self.main),
            Arc::clone(&self.aux),
            Arc::clone(&self.volume),
        ));
    }

    /// The music buffer, live.
    pub fn set_buffer_ms(&self, ms: u32) {
        if let Some(m) = self
            .mixer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            m.set_buffer_ms(ms);
        }
    }

    /// A session's sink. `big_endian_pcm`: CarPlay frames its LPCM uplink
    /// big-endian, Android Auto native.
    pub fn sink(&self, mic: Arc<Queue<MicFrame>>, big_endian_pcm: bool) -> Box<dyn Sinks> {
        Box::new(SessionSink {
            main: Arc::clone(&self.main),
            aux: Arc::clone(&self.aux),
            levels: Arc::clone(&self.levels),
            decoders: Mutex::new(Decoders::default()),
            mic_out: mic,
            mic: Mutex::new(None),
            device: Arc::clone(&self.mic_device),
            mic_gain: self.mic_gain,
            big_endian_pcm,
        })
    }

    /// A session ended: what is queued is dropped; the segment the mixer
    /// holds plays out (at most one period).
    pub fn clear(&self) {
        self.main.clear();
        self.aux.clear();
    }

    /// The phone dropped the media it had sent ahead (a pause, a skip):
    /// what is buffered of it must not play. Voice is left alone.
    pub fn clear_media(&self) {
        self.main.clear();
    }

    /// Shutdown: the device released, the unsaved level handed back.
    pub fn stop(&self) -> Option<u32> {
        if let Some(m) = self
            .mixer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            m.stop();
        }
        self.volume.close()
    }
}

/// One decoder per stream kind, reopened when its format changes, and
/// the segment they decode into: what is queued is a copy of what a
/// packet held (a few KB), not a segment of the largest size each.
struct Decoders {
    slots: [Option<(AudioFormat, Decoder)>; 4],
    scratch: Segment,
}

impl Default for Decoders {
    fn default() -> Self {
        Self {
            slots: Default::default(),
            scratch: Segment::with_capacity(SEGMENT_SAMPLES),
        }
    }
}

fn slot(kind: AudioKind) -> usize {
    match kind {
        AudioKind::Media => 0,
        AudioKind::Speech => 1,
        AudioKind::System => 2,
        AudioKind::Call => 3,
    }
}

struct SessionSink {
    main: Arc<Queue<Segment>>,
    aux: Arc<Queue<Segment>>,
    levels: Arc<[AtomicI32; 4]>,
    decoders: Mutex<Decoders>,
    mic_out: Arc<Queue<MicFrame>>,
    mic: Mutex<Option<Mic>>,
    /// The microphone's PCM, as the settings say now.
    device: Arc<Mutex<String>>,
    mic_gain: u32,
    big_endian_pcm: bool,
}

impl SessionSink {
    fn push(&self, kind: AudioKind, mut segment: Segment) {
        segment.scale(self.levels[slot(kind)].load(Ordering::Relaxed));
        let queue = match Input::of(kind) {
            Input::Main => &self.main,
            Input::Aux => &self.aux,
        };
        // Stale audio is worse than a gap: the oldest goes, the queue's own
        // counter feeds the stats line.
        queue.push_drop_oldest(segment);
    }
}

impl Sinks for SessionSink {
    fn audio_pcm(&mut self, kind: AudioKind, rate: u32, channels: u8, pcm: &[i16]) {
        let format = lehu_audio::Format::new(rate, channels);
        let mut segment = Segment::with_capacity(pcm.len().min(SEGMENT_SAMPLES));
        segment.format = format;
        if !segment.write(pcm) {
            log::warn!(
                "audio: a {kind:?} chunk of {} samples does not fit",
                pcm.len()
            );
            return;
        }
        self.push(kind, segment);
    }

    fn audio_packet(&mut self, kind: AudioKind, format: AudioFormat, payload: &[u8]) {
        let mut decoders = self.decoders.lock().unwrap_or_else(PoisonError::into_inner);
        let at = slot(kind);
        if decoders.slots[at]
            .as_ref()
            .is_none_or(|(f, _)| *f != format)
        {
            match Decoder::open(format) {
                Ok(d) => decoders.slots[at] = Some((format, d)),
                Err(e) => {
                    log::warn!("audio: {kind:?} {format:?}: {e}");
                    decoders.slots[at] = None;
                    return;
                }
            }
        }
        let Decoders { slots, scratch } = &mut *decoders;
        let Some((_, decoder)) = &mut slots[at] else {
            return;
        };
        scratch.clear();
        scratch.format = decoder.output();
        match decoder.decode(payload, scratch) {
            Ok(0) => {}
            Ok(_) => {
                let segment = Segment::from_slice(scratch.format, scratch.data());
                drop(decoders);
                self.push(kind, segment);
            }
            Err(e) => log::debug!("audio: {kind:?} packet: {e}"),
        }
    }

    fn mic(&mut self, wanted: Option<MicFormat>) {
        let mut mic = self.mic.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(m) = mic.take() {
            m.stop();
        }
        if let Some(format) = wanted {
            self.mic_out.clear();
            let device = self
                .device
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            match Mic::start(
                &device,
                format,
                self.mic_gain,
                self.big_endian_pcm,
                Arc::clone(&self.mic_out),
            ) {
                Ok(m) => *mic = Some(m),
                Err(e) => log::warn!("microphone: {e}"),
            }
        }
    }
}

impl Drop for SessionSink {
    fn drop(&mut self) {
        if let Some(m) = self
            .mic
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            m.stop();
        }
    }
}
