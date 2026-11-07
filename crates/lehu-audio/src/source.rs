//! One consumed stream: the segment being played, the queue behind it and
//! the interpolation state that carries across segment and period
//! boundaries.
//!
//! The hardware runs at one rate, 48 kHz stereo: the F1C codec is
//! 48k-native (the 44.1k family is not available) and has a single
//! playback stream. Every source is brought to it by a Q16
//! phase-accumulator linear resampler, integer only, and mono is
//! duplicated to stereo.

use std::sync::Arc;

use lehu_core::queue::Queue;

use crate::segment::{Format, Segment};

/// The mixer's rate, and the resampler's target.
pub const MIX_RATE: u32 = 48000;

/// Q15 duck ramp, one step per output frame (the rates FastCarPlay's
/// earlier PCM path used).
pub const FADE_IN_STEP_Q15: i32 = 1;
pub const FADE_OUT_STEP_Q15: i32 = 3;

pub struct Source {
    q: Arc<Queue<Segment>>,
    recycle: Option<Arc<Queue<Segment>>>,
    /// The segment being consumed, and the frames of it consumed so far.
    seg: Option<Segment>,
    pos: usize,
    frames: usize,
    /// A segment taken off the queue early (the idle wait pops to wake);
    /// consumed before the queue is read again.
    pending: Option<Segment>,
    format: Format,
    /// Q16 source frames per output frame.
    step: u32,
    /// Q16 position between prev and next.
    phase: u32,
    prev: (i16, i16),
    next: (i16, i16),
    /// prev and next hold valid samples.
    primed: bool,
    /// Consuming: the prefill is satisfied.
    pub live: bool,
    /// Consecutive periods with nothing to consume.
    pub dry_periods: u32,
    /// Not live with audio queued since (0: not waiting).
    pub wait_since: u64,
    /// |sample| OR-accumulator for silence detection over a period.
    pub zero_acc: u16,
}

impl Source {
    pub fn new(q: Arc<Queue<Segment>>, recycle: Option<Arc<Queue<Segment>>>) -> Self {
        Source {
            q,
            recycle,
            seg: None,
            pos: 0,
            frames: 0,
            pending: None,
            format: Format::STEREO_48K,
            step: 0,
            phase: 0,
            prev: (0, 0),
            next: (0, 0),
            primed: false,
            live: false,
            dry_periods: 0,
            wait_since: 0,
            zero_acc: 0,
        }
    }

    pub fn queue(&self) -> &Arc<Queue<Segment>> {
        &self.q
    }

    /// Hold a segment popped elsewhere as the next to play.
    pub fn pend(&mut self, seg: Segment) {
        if let Some(old) = self.pending.replace(seg) {
            self.give_back(old);
        }
    }

    /// Segments waiting: the queue's and the pending one.
    pub fn count(&self) -> usize {
        self.q.len() + usize::from(self.pending.is_some())
    }

    pub fn has(&self, n: usize) -> bool {
        self.count() >= n
    }

    /// Look at the next segment to play without taking it.
    pub fn head<R>(&self, f: impl FnOnce(Option<&Segment>) -> R) -> R {
        match &self.pending {
            Some(p) => f(Some(p)),
            None => self.q.peek(f),
        }
    }

    /// Take the next segment to play.
    fn take_head(&mut self) -> Option<Segment> {
        self.pending.take().or_else(|| self.q.pop())
    }

    /// Drop the oldest waiting segment; false when none waited.
    pub fn drop_oldest(&mut self) -> bool {
        match self.take_head() {
            Some(s) => {
                self.give_back(s);
                true
            }
            None => false,
        }
    }

    /// Forget the format, so the next segment re-primes the interpolator.
    pub fn unprime(&mut self) {
        self.primed = false;
    }

    /// A consumed segment back to its producer, cleared; freed when there
    /// is no recycle queue or it is full.
    fn give_back(&self, mut seg: Segment) {
        if let Some(r) = &self.recycle {
            seg.clear();
            r.push_discard(seg);
        }
    }

    /// Audio waiting to play, the current segment's rest included, in ms.
    pub fn queued_ms(&self) -> u32 {
        let mut ms = 0u64;
        if let Some(s) = &self.seg
            && self.pos < self.frames
        {
            ms += u64::from(s.format.ms_of(self.frames - self.pos));
        }
        // Segments of one stream are the same size: the head stands for
        // all.
        let count = self.count() as u64;
        ms += self.head(|h| match h {
            Some(h) if h.format.rate > 0 => {
                h.frames() as u64 * count * 1000 / u64::from(h.format.rate)
            }
            _ => 0,
        });
        ms.min(u64::from(u32::MAX)) as u32
    }

    fn next_segment(&mut self) -> bool {
        if let Some(old) = self.seg.take() {
            self.give_back(old);
        }
        let Some(seg) = self.take_head() else {
            self.pos = 0;
            self.frames = 0;
            return false;
        };
        if seg.format != self.format || self.step == 0 {
            self.format = seg.format;
            self.step = ((u64::from(seg.format.rate) << 16) / u64::from(MIX_RATE)) as u32;
            // A format change re-primes the interpolator.
            self.primed = false;
        }
        self.frames = seg.frames();
        self.pos = 0;
        self.seg = Some(seg);
        true
    }

    /// One source frame (L/R native S16); None when the source ran dry.
    fn fetch_frame(&mut self) -> Option<(i16, i16)> {
        while self.seg.is_none() || self.pos >= self.frames {
            if !self.next_segment() {
                return None;
            }
        }
        let Some(seg) = &self.seg else {
            return None;
        };
        let ch = usize::from(self.format.channels);
        let data = seg.data();
        let i = self.pos * ch;
        let l = data[i];
        let r = if ch == 2 { data[i + 1] } else { l };
        self.pos += 1;
        self.zero_acc |= (l as u16) | (r as u16);
        Some((l, r))
    }

    /// Resample `frames` output frames into the stereo accumulator `mix`
    /// (2 * `frames` long) with a per-frame Q15 gain ramp from `vol`
    /// towards `target`. The frames actually rendered; the rest stay silent
    /// when the source runs dry mid-period.
    pub fn render(&mut self, mix: &mut [i32], frames: usize, vol: &mut i32, target: i32) -> usize {
        let frames = frames.min(mix.len() / 2);
        // The usual case: 48 kHz music, no fade under way. Each output
        // frame is the next source frame, the one fetched ahead (the phase
        // stays 0): the same output as the interpolator below, without its
        // arithmetic.
        if self.step == 65536 && self.primed && self.phase == 0 && *vol == target {
            let v = *vol;
            for i in 0..frames {
                let (l, r) = (i32::from(self.next.0), i32::from(self.next.1));
                let got = self.fetch_frame();
                let (ol, or) = if v == 32768 {
                    (l, r)
                } else {
                    ((l * v) >> 15, (r * v) >> 15)
                };
                mix[2 * i] += ol;
                mix[2 * i + 1] += or;
                match got {
                    Some(n) => {
                        self.prev = self.next;
                        self.next = n;
                    }
                    None => {
                        // Dry: this frame still plays, as below.
                        self.primed = false;
                        return i + 1;
                    }
                }
            }
            return frames;
        }

        for i in 0..frames {
            if *vol < target {
                *vol = (*vol + FADE_IN_STEP_Q15).min(target);
            } else if *vol > target {
                *vol = (*vol - FADE_OUT_STEP_Q15).max(target);
            }

            if !self.primed {
                let Some(p) = self.fetch_frame() else {
                    return i;
                };
                self.prev = p;
                self.next = self.fetch_frame().unwrap_or(p);
                self.phase = 0;
                self.primed = true;
            }

            self.phase += self.step;
            while self.phase >= 65536 {
                self.phase -= 65536;
                self.prev = self.next;
                match self.fetch_frame() {
                    Some(n) => self.next = n,
                    None => {
                        // Dry mid-run: emit what we have and re-prime on
                        // resume.
                        self.primed = false;
                        self.phase = 0;
                        break;
                    }
                }
            }

            // Linear interpolation; the 32x32 product is kept exact.
            let ph = i64::from(self.phase);
            let dl = i64::from(self.next.0) - i64::from(self.prev.0);
            let dr = i64::from(self.next.1) - i64::from(self.prev.1);
            let l = i32::from(self.prev.0) + ((dl * ph) >> 16) as i32;
            let r = i32::from(self.prev.1) + ((dr * ph) >> 16) as i32;
            mix[2 * i] += (l * *vol) >> 15;
            mix[2 * i + 1] += (r * *vol) >> 15;
        }
        frames
    }

    /// With the device closed: drop waiting segments of digital silence
    /// (every sample within `level`); true when audible audio waits.
    pub fn drop_silence(&mut self, level: i16) -> bool {
        loop {
            let silent = self.head(|h| h.map(|s| s.is_silent(level)));
            match silent {
                None => return false,
                Some(false) => return true,
                Some(true) => {
                    self.drop_oldest();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(format: Format, chunks: &[&[i16]]) -> Source {
        let q = Arc::new(Queue::new(16));
        for c in chunks {
            q.push_discard(Segment::from_slice(format, c));
        }
        Source::new(q, None)
    }

    fn render(s: &mut Source, frames: usize, vol: i32, target: i32) -> (Vec<i32>, usize, i32) {
        let mut mix = vec![0i32; 2 * frames];
        let mut v = vol;
        let n = s.render(&mut mix, frames, &mut v, target);
        (mix, n, v)
    }

    #[test]
    fn at_48k_the_output_is_the_input_one_frame_late() {
        let step: Vec<i16> = (0..8).flat_map(|i| [i * 1000, -i * 1000]).collect();
        let mut s = source(Format::STEREO_48K, &[&step]);
        s.live = true;
        let (mix, n, _) = render(&mut s, 8, 32768, 32768);
        // Priming takes the first frame as "prev" and never plays it (as
        // FastCarPlay did; one frame, inaudible); the last frame plays as
        // the source runs dry, so 7 of the 8 come out.
        assert_eq!(n, 7);
        let l: Vec<i32> = mix.iter().step_by(2).copied().collect();
        assert_eq!(l, vec![1000, 2000, 3000, 4000, 5000, 6000, 7000, 0]);
        let r: Vec<i32> = mix.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(r[6], -7000);
        // The fast path then takes over once primed at unity.
        let mut s = source(Format::STEREO_48K, &[&step, &step]);
        let (a, _, _) = render(&mut s, 4, 32768, 32768);
        let (b, n, _) = render(&mut s, 4, 32768, 32768);
        assert_eq!(n, 4);
        assert_eq!(
            a[0..8],
            [1000, -1000, 2000, -2000, 3000, -3000, 4000, -4000]
        );
        assert_eq!(b[0..8], [5000, -5000, 6000, -6000, 7000, -7000, 0, 0]);
    }

    #[test]
    fn a_16k_ramp_is_interpolated_to_48k() {
        // Mono 16 kHz ramp 0, 3000, 6000, ...: at 48 kHz every output frame
        // steps by 1000, and mono lands on both channels.
        let ramp: Vec<i16> = (0..6).map(|i| i * 3000).collect();
        let mut s = source(Format::MONO_16K, &[&ramp]);
        let (mix, n, _) = render(&mut s, 9, 32768, 32768);
        assert_eq!(n, 9);
        let l: Vec<i32> = mix.iter().step_by(2).copied().collect();
        let r: Vec<i32> = mix.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(l, r);
        for (i, v) in l.iter().enumerate().take(8) {
            let want = (i as i32 + 1) * 1000;
            assert!((v - want).abs() <= 1, "frame {i}: {v} vs {want}");
        }
    }

    #[test]
    fn the_fade_ramps_the_gain_per_frame() {
        let flat = vec![10000i16; 2 * 16];
        let mut s = source(Format::STEREO_48K, &[&flat]);
        let (mix, _, v) = render(&mut s, 4, 32768 - 4 * 3, 32768);
        assert_eq!(v, 32768 - 8);
        // Gains 32757..32760 applied to 10000: all below 10000, rising.
        let l: Vec<i32> = mix.iter().step_by(2).copied().collect();
        assert!(l.windows(2).all(|w| w[0] <= w[1]));
        assert!(l[0] < 10000 && l[3] < 10000);
        let (_, _, v) = render(&mut s, 4, 32768, 32768 - 100);
        assert_eq!(v, 32768 - 12, "fade out at 3 per frame");
    }

    #[test]
    fn running_dry_reports_the_rendered_count_and_resumes() {
        let a = vec![5000i16; 2 * 3];
        let mut s = source(Format::STEREO_48K, &[&a]);
        let (mix, n, _) = render(&mut s, 8, 32768, 32768);
        assert_eq!(n, 2, "three frames, one taken by the priming");
        assert_eq!(mix[4], 0, "the rest stays silent");
        assert_eq!(s.queued_ms(), 0);
        let (_, n, _) = render(&mut s, 8, 32768, 32768);
        assert_eq!(n, 0);
        s.queue()
            .push_discard(Segment::from_slice(Format::STEREO_48K, &a));
        let (mix, n, _) = render(&mut s, 8, 32768, 32768);
        assert_eq!(n, 2);
        assert_eq!(mix[0], 5000);
    }

    #[test]
    fn queued_ms_counts_the_rest_and_the_queue() {
        let seg = vec![0i16; 2 * 480]; // 10 ms stereo at 48k
        let mut s = source(Format::STEREO_48K, &[&seg, &seg, &seg]);
        assert_eq!(s.queued_ms(), 30);
        render(&mut s, 240, 32768, 32768);
        // Half of the first segment played (plus the two primed frames).
        assert_eq!(s.queued_ms(), 24);
        s.pend(Segment::from_slice(Format::STEREO_48K, &seg));
        assert_eq!(s.count(), 3);
        assert!(s.has(3));
        assert_eq!(s.queued_ms(), 34);
    }

    #[test]
    fn silence_is_dropped_until_something_audible() {
        let quiet = vec![3i16; 4];
        let loud = vec![0i16, 0, 500, 0];
        let mut s = source(Format::STEREO_48K, &[&quiet, &quiet, &loud, &quiet]);
        assert!(s.drop_silence(4));
        assert_eq!(s.count(), 2);
        s.queue().clear();
        assert_eq!(s.count(), 0);
        assert!(!s.drop_silence(4));
    }

    #[test]
    fn consumed_segments_are_recycled_cleared() {
        let q = Arc::new(Queue::new(4));
        let recycle = Arc::new(Queue::new(4));
        q.push_discard(Segment::from_slice(Format::STEREO_48K, &[1, 2, 3, 4]));
        q.push_discard(Segment::from_slice(Format::STEREO_48K, &[5, 6, 7, 8]));
        let mut s = Source::new(q, Some(recycle.clone()));
        render(&mut s, 8, 32768, 32768);
        assert_eq!(recycle.len(), 2, "both segments came back");
        let back = recycle.pop().expect("recycled");
        assert!(back.is_empty());
        assert_eq!(back.capacity(), 4);
        assert_eq!(recycle.len(), 1);
    }

    #[test]
    fn a_format_change_reprimes() {
        let a = vec![1000i16; 2 * 4];
        let b: Vec<i16> = vec![2000i16; 8];
        let q = Arc::new(Queue::new(4));
        q.push_discard(Segment::from_slice(Format::STEREO_48K, &a));
        q.push_discard(Segment::from_slice(Format::MONO_16K, &b));
        let mut s = Source::new(q, None);
        let (mix, n, _) = render(&mut s, 4 + 24, 32768, 32768);
        // 3 of the 4 stereo frames, then the 16 kHz segment re-primed (two
        // frames taken) and 5 frames stretched to 15, plus the one played
        // when it runs dry.
        assert_eq!(n, 22);
        assert_eq!(mix[0], 1000);
        assert_eq!(mix[2 * 21], 2000);
        assert_eq!(mix[2 * 21 + 1], 2000);
    }
}
