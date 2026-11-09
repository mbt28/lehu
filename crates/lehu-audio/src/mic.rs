// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The microphone: blocking capture from the ADC on its own thread, framed
//! and coded as the phone asked (`MicFormat`), each frame pushed into the
//! session's microphone queue for the protocol to seal and send.
//!
//! The read sleeps until one frame of samples exists, so the thread wakes
//! once per frame; the driver's ring is the only buffer and cannot build a
//! stale backlog (an overrun surfaces as an error the recovery clears with
//! a small gap). FastCarPlay's earlier path through a queued-audio layer
//! polled and burned half the core.
//!
//! Two things learned on the board are kept: the sun4i codec shares one
//! PLL between the DAC and the ADC, so capture always runs at 48 kHz when
//! the wanted rate divides it (opening the ADC at 16 or 24 kHz against the
//! mixer's 48 kHz playback is refused) and is decimated here; and the ADC
//! has a large fixed DC bias (about -2300 LSB) that the phone's gain
//! control amplifies into distortion, so a one-pole high-pass centres the
//! signal before the gain.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use lehu_core::queue::Queue;
use lehu_session::{AudioCodec, MicFormat, MicFrame};

use crate::Error;
use crate::alsa::{self, Pcm};
use crate::opus::OpusEncoder;
use crate::priority;

/// The capture rate when the wanted rate divides it.
const CAPTURE_RATE: u32 = 48000;
/// Periods of ring in the driver.
const RING_PERIODS: usize = 8;
/// After a failed read: an unhappy device must not spin the core.
const RETRY_WAIT: Duration = Duration::from_millis(50);

/// Samples in one frame of `format`: 7.5 ms at 48 kHz is 360, 20 ms at
/// 16 kHz is 320.
pub fn frame_samples(format: MicFormat) -> usize {
    (format.rate as usize * usize::from(format.frame_tenths_ms)) / 10_000
}

/// Capture frames per wanted frame: 3 at 16 kHz, 2 at 24 kHz, 1 at 48.
fn decimation(rate: u32) -> usize {
    if rate > 0 && CAPTURE_RATE.is_multiple_of(rate) {
        (CAPTURE_RATE / rate) as usize
    } else {
        1
    }
}

/// Every group of `decim` capture frames averaged into one sample, channel
/// 0 when the hardware insisted on stereo. Crude, but adequate anti-alias
/// for voice. `dst.len()` output samples.
pub fn decimate(src: &[i16], channels: usize, decim: usize, dst: &mut [i16]) {
    let channels = channels.max(1);
    let decim = decim.max(1);
    for (i, d) in dst.iter_mut().enumerate() {
        let base = i * decim * channels;
        let mut acc: i32 = 0;
        for k in 0..decim {
            acc += i32::from(src.get(base + k * channels).copied().unwrap_or(0));
        }
        // Constant divisors, so the compiler reduces them to multiplies on
        // a core with no divide.
        *d = match decim {
            1 => acc,
            2 => acc / 2,
            3 => acc / 3,
            n => acc / n as i32,
        } as i16;
    }
}

/// A one-pole high-pass, about 8 Hz at 48 kHz: y[n] = x[n] - x[n-1] +
/// R y[n-1], R = 0.999 in Q15. Removes the ADC's bias and rumble without
/// touching the voice band; its state carries across frames. The state
/// keeps 16 fractional bits: with the product floored to whole samples
/// the decay stalls at about -990 LSB for a negative bias (the F1C's is),
/// which is most of what the filter is there to remove.
#[derive(Default)]
pub struct DcBlocker {
    x1: i32,
    y1_q16: i64,
}

impl DcBlocker {
    const R_Q15: i64 = 32735;

    /// Block the DC, then apply the integer `gain` (1 = unchanged), both
    /// saturating. The high-pass comes first so the gain acts on centred
    /// audio and does not amplify the offset into clipping.
    pub fn process(&mut self, samples: &mut [i16], gain: u32) {
        let gain = i32::try_from(gain.max(1)).unwrap_or(i32::MAX);
        for s in samples {
            let x = i32::from(*s);
            let y_q16 = (i64::from(x - self.x1) << 16) + ((Self::R_Q15 * self.y1_q16) >> 15);
            self.x1 = x;
            self.y1_q16 = y_q16;
            let y = ((y_q16 + (1 << 15)) >> 16) as i32;
            let v = if gain > 1 { y.saturating_mul(gain) } else { y };
            *s = v.clamp(-32768, 32767) as i16;
        }
    }
}

/// S16 samples to big-endian bytes (CarPlay's LPCM wire order).
pub fn to_big_endian(pcm: &[i16], out: &mut [u8]) -> usize {
    let n = pcm.len().min(out.len() / 2);
    for (s, b) in pcm[..n].iter().zip(out.as_chunks_mut::<2>().0.iter_mut()) {
        b.copy_from_slice(&s.to_be_bytes());
    }
    2 * n
}

/// S16 samples to native (little-endian) bytes.
pub fn to_native(pcm: &[i16], out: &mut [u8]) -> usize {
    let n = pcm.len().min(out.len() / 2);
    for (s, b) in pcm[..n].iter().zip(out.as_chunks_mut::<2>().0.iter_mut()) {
        b.copy_from_slice(&s.to_le_bytes());
    }
    2 * n
}

enum Coder {
    /// Big-endian when `big_endian` (CarPlay), native otherwise.
    Pcm {
        big_endian: bool,
    },
    Opus(OpusEncoder),
}

pub struct Mic {
    active: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Mic {
    /// Start capturing `device` as `format` asks, with the `mic-gain`
    /// setting, frames into `out` (the newest kept when it is full, as
    /// stale voice is worse than a gap). PCM frames are big-endian when
    /// `big_endian_pcm` (CarPlay's LPCM), native otherwise (Android Auto).
    /// A format the microphone cannot produce is refused here; a device
    /// that cannot be opened is logged from the thread.
    pub fn start(
        device: &str,
        format: MicFormat,
        gain: u32,
        big_endian_pcm: bool,
        out: Arc<Queue<MicFrame>>,
    ) -> Result<Mic, Error> {
        let frame = frame_samples(format);
        if frame == 0 || 2 * frame > MicFrame::CAPACITY {
            return Err(Error::Format("microphone frame length"));
        }
        let coder = match format.codec {
            AudioCodec::Pcm => Coder::Pcm {
                big_endian: big_endian_pcm,
            },
            AudioCodec::Opus => Coder::Opus(OpusEncoder::new(format.rate)?),
            AudioCodec::AacLc => return Err(Error::Format("microphone codec: PCM or Opus")),
        };
        let decim = decimation(format.rate);
        let active = Arc::new(AtomicBool::new(true));
        let mut worker = Worker {
            device: device.to_string(),
            active: active.clone(),
            rate: format.rate,
            frame,
            decim,
            gain,
            coder,
            out,
            capture: vec![0; frame * decim * 2],
            pcm: vec![0; frame],
            encoded: [0; MicFrame::CAPACITY],
            dc: DcBlocker::default(),
        };
        let thread = std::thread::Builder::new()
            .name("audio-mic".into())
            .spawn(move || worker.run())
            .map_err(|_| Error::Format("cannot start the microphone thread"))?;
        Ok(Mic {
            active,
            thread: Some(thread),
        })
    }

    /// Stop capturing and release the device; returns once joined (within
    /// a frame or a retry wait).
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

impl Drop for Mic {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Worker {
    device: String,
    active: Arc<AtomicBool>,
    rate: u32,
    frame: usize,
    decim: usize,
    gain: u32,
    coder: Coder,
    out: Arc<Queue<MicFrame>>,
    /// One capture period: frame * decim frames, up to stereo.
    capture: Vec<i16>,
    /// One frame at the wanted rate, mono.
    pcm: Vec<i16>,
    encoded: [u8; MicFrame::CAPACITY],
    dc: DcBlocker,
}

impl Worker {
    fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn run(&mut self) {
        priority::audio_thread("mic");
        let cap_rate = if self.decim > 1 {
            CAPTURE_RATE
        } else {
            self.rate
        };
        let period = self.frame * self.decim;
        let (pcm, got) =
            match Pcm::open_capture(&self.device, cap_rate, period, period * RING_PERIODS) {
                Ok(x) => x,
                Err(e) => {
                    log::error!(
                        "mic: capture open {} failed: {e}; no microphone",
                        self.device
                    );
                    return;
                }
            };
        log::info!(
            "mic: ALSA capture {} Hz {}ch /{} -> {} Hz, period {}, ring {}, {}-sample {} frames",
            got.rate,
            got.channels,
            self.decim,
            self.rate,
            got.period,
            got.buffer,
            self.frame,
            match self.coder {
                Coder::Pcm { .. } => "PCM",
                Coder::Opus(_) => "Opus",
            }
        );
        let channels = got.channels as usize;
        let mut sent = 0u64;
        while self.active() {
            // Block until one frame's worth of samples exists: the read is
            // the loop's pacing.
            let got = match pcm.read(&mut self.capture, period) {
                Ok(n) => n,
                Err(err) => {
                    if let Err(e) = pcm.recover(err) {
                        log::warn!("mic: capture failed: {}", alsa::strerror(e));
                    }
                    std::thread::sleep(RETRY_WAIT);
                    continue;
                }
            };
            if got < period {
                continue; // a short read around a recovery: resync on the next
            }
            if channels == 1 && self.decim == 1 {
                self.pcm.copy_from_slice(&self.capture[..self.frame]);
            } else {
                decimate(&self.capture, channels, self.decim, &mut self.pcm);
            }
            self.dc.process(&mut self.pcm, self.gain);
            let len = match &mut self.coder {
                Coder::Pcm { big_endian: true } => to_big_endian(&self.pcm, &mut self.encoded),
                Coder::Pcm { big_endian: false } => to_native(&self.pcm, &mut self.encoded),
                Coder::Opus(enc) => match enc.encode(&self.pcm, &mut self.encoded) {
                    Ok(n) => n,
                    Err(e) => {
                        log::warn!("mic: {e}");
                        continue;
                    }
                },
            };
            if let Some(frame) = MicFrame::from_slice(&self.encoded[..len]) {
                self.out.push_drop_oldest(frame);
                if sent == 0 {
                    log::info!("mic: first frame captured ({len} bytes)");
                }
                sent += 1;
            }
        }
        drop(pcm);
        log::info!("mic: stopped ({sent} frames)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(codec: AudioCodec, rate: u32, tenths: u16) -> MicFormat {
        MicFormat {
            codec,
            rate,
            frame_tenths_ms: tenths,
        }
    }

    #[test]
    fn framing_follows_the_phone() {
        // CarPlay's wired LPCM: 7.5 ms packets, 360 samples at 48 kHz and
        // 180 at 24 kHz (20 ms packets made the phone discard the mic).
        assert_eq!(frame_samples(fmt(AudioCodec::Pcm, 48000, 75)), 360);
        assert_eq!(frame_samples(fmt(AudioCodec::Pcm, 24000, 75)), 180);
        assert_eq!(frame_samples(fmt(AudioCodec::Pcm, 16000, 75)), 120);
        // Opus keeps 20 ms, a valid Opus duration.
        assert_eq!(frame_samples(fmt(AudioCodec::Opus, 16000, 200)), 320);
        assert_eq!(frame_samples(fmt(AudioCodec::Opus, 24000, 200)), 480);
        assert_eq!(decimation(16000), 3);
        assert_eq!(decimation(24000), 2);
        assert_eq!(decimation(48000), 1);
        assert_eq!(decimation(44100), 1);
        assert_eq!(decimation(0), 1);
    }

    #[test]
    fn decimation_averages_and_takes_the_left_channel() {
        let src = [300i16, 9, 600, 9, 900, 9, 30, 9, 60, 9, 90, 9];
        let mut dst = [0i16; 2];
        decimate(&src, 2, 3, &mut dst);
        assert_eq!(dst, [600, 60]);
        let mono = [100i16, 200, 300, 400];
        let mut dst = [0i16; 2];
        decimate(&mono, 1, 2, &mut dst);
        assert_eq!(dst, [150, 350]);
        let mut dst = [0i16; 4];
        decimate(&mono, 1, 1, &mut dst);
        assert_eq!(dst, mono);
        // Short input reads as zeros rather than out of bounds.
        let mut dst = [7i16; 3];
        decimate(&mono[..2], 1, 1, &mut dst);
        assert_eq!(dst, [100, 200, 0]);
    }

    #[test]
    fn the_dc_blocker_removes_the_bias_and_keeps_the_voice() {
        let mut dc = DcBlocker::default();
        // The ADC's offset: a constant -2300 settles to nothing.
        let mut flat = vec![-2300i16; 48000];
        dc.process(&mut flat, 1);
        assert_eq!(flat[0], -2300, "the step passes at first");
        assert!(flat[47999].abs() <= 1, "{}", flat[47999]);
        // A 1 kHz tone on the same offset comes through at its amplitude
        // (a 1 kHz square wave, integer only).
        let mut dc = DcBlocker::default();
        let mut tone: Vec<i16> = (0..48000)
            .map(|i| {
                if (i / 24) % 2 == 0 {
                    -2300 + 4000
                } else {
                    -2300 - 4000
                }
            })
            .collect();
        dc.process(&mut tone, 1);
        let tail = &tone[40000..];
        let max = tail.iter().copied().max().unwrap_or(0);
        let min = tail.iter().copied().min().unwrap_or(0);
        assert!((3900..=4100).contains(&max), "{max}");
        assert!((-4100..=-3900).contains(&min), "{min}");
        // The gain multiplies and saturates.
        let mut dc = DcBlocker::default();
        let mut loud = [20000i16, 20000];
        dc.process(&mut loud, 4);
        assert_eq!(loud[0], 32767);
        let mut dc = DcBlocker::default();
        let mut quiet = [100i16];
        dc.process(&mut quiet, 3);
        assert_eq!(quiet[0], 300);
    }

    #[test]
    fn byte_orders() {
        let mut out = [0u8; 6];
        assert_eq!(to_big_endian(&[0x0102, -2], &mut out), 4);
        assert_eq!(out[..4], [0x01, 0x02, 0xff, 0xfe]);
        assert_eq!(to_native(&[0x0102, -2], &mut out), 4);
        assert_eq!(out[..4], [0x02, 0x01, 0xfe, 0xff]);
        assert_eq!(to_native(&[1, 2, 3, 4], &mut out), 6, "cut to the room");
    }

    #[test]
    fn formats_are_checked_before_the_device() {
        let out = Arc::new(Queue::new(4));
        assert!(matches!(
            Mic::start(
                "none",
                fmt(AudioCodec::AacLc, 16000, 200),
                1,
                true,
                out.clone()
            ),
            Err(Error::Format(_))
        ));
        assert!(matches!(
            Mic::start("none", fmt(AudioCodec::Pcm, 48000, 0), 1, true, out.clone()),
            Err(Error::Format(_))
        ));
        assert!(matches!(
            Mic::start(
                "none",
                fmt(AudioCodec::Pcm, 48000, 400),
                1,
                true,
                out.clone()
            ),
            Err(Error::Format(_)),
        ));
        // A PCM mic on a device that does not exist starts and stops
        // cleanly; the thread reports the device.
        let mic = Mic::start(
            "lehu-test-no-such-device",
            fmt(AudioCodec::Pcm, 16000, 75),
            1,
            true,
            out.clone(),
        )
        .expect("pcm needs no library");
        std::thread::sleep(Duration::from_millis(50));
        mic.stop();
        assert!(out.is_empty());
    }
}
