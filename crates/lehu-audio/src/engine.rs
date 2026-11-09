// SPDX-License-Identifier: GPL-3.0-or-later
//! The mixer's work for one period, with no device in sight: the two
//! sources rendered, ducked, volumed and clamped into one period of S16
//! stereo, and the decisions around it (prefill, the jitter buffer, the
//! latency cap, silence). The thread in `mixer.rs` owns the device and
//! calls this once per period; the tests call it with their own clock.

use std::sync::Arc;

use lehu_core::queue::Queue;

use crate::segment::Segment;
use crate::source::Source;
use crate::stats::Stats;

pub const MIX_CHANNELS: usize = 2;
/// Period and buffer mirror what the kernel driver was validated with:
/// 1024/4096 frames, about 21 ms and 85 ms.
pub const MIX_PERIOD: usize = 1024;
pub const MIX_BUFFER: usize = 4096;
pub const MIX_PERIOD_MS: u32 = (1000 * MIX_PERIOD as u32) / crate::source::MIX_RATE;

/// Software volume ramp per output frame: a full swing in about 10 ms, no
/// click.
const VOLUME_STEP_Q15: i32 = 64;
/// The voice source silent this many periods before the music ramps back
/// (about 85 ms, the old 10-segment release at 7.5 ms voice segments).
const AUX_SILENT_RELEASE_PERIODS: u32 = 4;
/// A live source dry this long reverts to needing a prefill; burst gaps
/// shorter than this resume at once on the hardware buffer.
const DRY_RELEASE_PERIODS: u32 = 24;
/// Samples at or below this are digital silence (-78 dBFS): what a phone
/// sends while a stream stays open and nothing plays.
pub const SILENCE_LEVEL: i16 = 4;
/// The music buffer's range: the music queue holds about 2.7 s, room for a
/// burst on top of the deepest.
pub const MAX_BUFFER_MS: u32 = 1500;
/// Voice other than a call (navigation, the assistant) starts after this
/// many segments.
const AUX_PREFILL_SEGMENTS: usize = 2;
/// Music queued this far past its buffer is a backlog, what a phone sends
/// in one go once a stalled link recovers, not jitter: it is skipped down
/// to the buffer, so the music plays at the live edge again instead of
/// seconds late. Voice is capped much tighter (`voice_max_latency_ms`),
/// since a prompt heard late is wrong; music heard late is only late.
const LATENCY_SLACK_MS: u32 = 1000;

/// The settings the engine runs with, fixed at start.
#[derive(Clone, Copy, Debug)]
pub struct Tuning {
    /// Where ducked music settles, Q15 (the `fade` setting).
    pub fade_floor_q15: i32,
    /// Voice segments held before a call's audio starts.
    pub call_prefill_segments: usize,
    /// Voice older than this is dropped rather than played late.
    pub voice_max_latency_ms: u32,
    /// The device is released after this much silence.
    pub idle_hold_ms: u32,
}

/// What a period decided.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Period {
    /// Only digital silence was written.
    pub silent: bool,
    /// The device has been idle past `idle_hold_ms`: close it.
    pub release: bool,
}

pub struct Engine {
    pub main: Source,
    pub aux: Source,
    /// Ducked by the voice.
    main_vol_q15: i32,
    main_target_q15: i32,
    /// The software volume, as ramped so far.
    volume_q15: i32,
    aux_silent_periods: u32,
    /// The music buffer applied so far.
    buffer_ms: Option<u32>,
    idle_periods: u32,
    tuning: Tuning,
    pub stats: Stats,
    mix: Vec<i32>,
}

impl Engine {
    pub fn new(
        main: Arc<Queue<Segment>>,
        aux: Arc<Queue<Segment>>,
        recycle: Option<Arc<Queue<Segment>>>,
        tuning: Tuning,
    ) -> Self {
        Engine {
            main: Source::new(main, recycle.clone()),
            aux: Source::new(aux, recycle),
            main_vol_q15: 32768,
            main_target_q15: 32768,
            volume_q15: 32768,
            aux_silent_periods: AUX_SILENT_RELEASE_PERIODS,
            buffer_ms: None,
            idle_periods: 0,
            tuning,
            stats: Stats::new(),
            mix: vec![0; MIX_PERIOD * MIX_CHANNELS],
        }
    }

    /// The device was opened. Nothing was playing: the software volume
    /// starts at its level, not ramping from an old one.
    pub fn on_open(&mut self, soft_gain_q15: i32) {
        self.main_vol_q15 = 32768;
        self.main_target_q15 = 32768;
        self.aux_silent_periods = AUX_SILENT_RELEASE_PERIODS;
        self.idle_periods = 0;
        self.volume_q15 = soft_gain_q15;
        self.stats.reset();
    }

    /// The device was closed: both sources prefill again before playing.
    pub fn on_close(&mut self) {
        for s in [&mut self.main, &mut self.aux] {
            s.live = false;
            s.unprime();
        }
    }

    /// With the device closed: drop queued silence; true when audible
    /// audio waits in either source.
    pub fn drop_silence(&mut self) -> bool {
        let main = self.main.drop_silence(SILENCE_LEVEL);
        let aux = self.aux.drop_silence(SILENCE_LEVEL);
        main || aux
    }

    /// The music buffer applied, for the stats line.
    pub fn buffer_ms(&self) -> u32 {
        self.buffer_ms.unwrap_or(0)
    }

    /// Once a period while the device is open: the stats line when due.
    pub fn report(&mut self, now_ms: u64) -> Option<String> {
        let depth = self.main.queued_ms();
        let drops = self
            .main
            .queue()
            .drops()
            .wrapping_add(self.aux.queue().drops());
        let buffer = self.buffer_ms();
        self.stats.tick(now_ms, depth, drops, buffer)
    }

    /// The DAC ran out.
    pub fn underrun(&mut self) {
        self.stats.underruns += 1;
    }

    /// The music buffer, followed live: deeper refills once (a short pause
    /// while it fills to the new depth), shallower drops the excess at
    /// once, the oldest first, so it plays sooner from here.
    fn follow_buffer(&mut self, buffer_ms: u32) {
        if self.buffer_ms == Some(buffer_ms) {
            return;
        }
        if let Some(applied) = self.buffer_ms
            && self.main.live
        {
            if buffer_ms > applied {
                self.main.live = false;
            } else {
                while self.main.queued_ms() > buffer_ms && self.main.drop_oldest() {}
            }
        }
        self.buffer_ms = Some(buffer_ms);
    }

    /// Drop the oldest voice segments once the queued backlog exceeds the
    /// latency cap: the bounded queue and the small hardware buffer limit
    /// the worst case; this trims a sustained lead.
    fn cap_aux(&mut self) {
        let seg_ms = self.aux.head(|h| h.map_or(0, Segment::duration_ms));
        if seg_ms == 0 {
            return;
        }
        let cap = self.tuning.voice_max_latency_ms;
        while (self.aux.count() as u32).saturating_mul(seg_ms) > cap
            && self.aux.count() > 1
            && self.aux.drop_oldest()
        {}
    }

    /// One period: `buffer_setting_ms` is the live setting, `soft_gain_q15`
    /// the volume's software gain, `out` the period to write (`MIX_PERIOD`
    /// stereo frames).
    pub fn period(
        &mut self,
        now_ms: u64,
        buffer_setting_ms: u32,
        soft_gain_q15: i32,
        out: &mut [i16],
    ) -> Period {
        let buffer_ms = buffer_setting_ms.min(MAX_BUFFER_MS);
        self.follow_buffer(buffer_ms);
        let queued = self.main.queued_ms();
        if queued > buffer_ms + LATENCY_SLACK_MS {
            while self.main.queued_ms() > buffer_ms && self.main.drop_oldest() {}
            self.stats.skipped_ms += queued - self.main.queued_ms();
        }

        // Liveness: a source that is not playing waits for a prefill
        // before it starts, which absorbs bursty delivery. Music waits for
        // its buffer, or as long, should less ever come (a clip shorter
        // than the buffer still plays); a call and other voice for a few
        // segments.
        let call_prefill = self.tuning.call_prefill_segments;
        for (is_main, s) in [(true, &mut self.main), (false, &mut self.aux)] {
            if s.live {
                continue;
            }
            if !s.has(1) {
                s.wait_since = 0;
                continue;
            }
            if s.wait_since == 0 {
                s.wait_since = now_ms.max(1);
            }
            let ready = if is_main {
                s.queued_ms() >= buffer_ms
                    || now_ms.saturating_sub(s.wait_since) >= u64::from(buffer_ms)
            } else {
                let mono = s.head(|h| h.is_some_and(|h| h.format.channels == 1));
                s.has(if mono {
                    call_prefill
                } else {
                    AUX_PREFILL_SEGMENTS
                })
            };
            if ready {
                s.live = true;
                s.dry_periods = 0;
                s.wait_since = 0;
            }
        }

        self.cap_aux();

        self.mix.fill(0);
        self.aux.zero_acc = 0;
        let mut main_rendered = 0;
        if self.main.live {
            main_rendered = self.main.render(
                &mut self.mix,
                MIX_PERIOD,
                &mut self.main_vol_q15,
                self.main_target_q15,
            );
        }
        // Music that runs dry refills to its buffer before it plays on: one
        // pause, then a full buffer again, not a stutter with every late
        // packet that follows.
        if self.main.live && main_rendered < MIX_PERIOD {
            self.main.live = false;
            self.stats.dry += 1;
        }
        let mut aux_rendered = 0;
        if self.aux.live {
            let mut unity = 32768;
            aux_rendered = self
                .aux
                .render(&mut self.mix, MIX_PERIOD, &mut unity, 32768);
        }

        // Duck: audible voice pulls the music down to the fade floor; after
        // the release window of silent or dry periods it ramps back up.
        if aux_rendered > 0 && self.aux.zero_acc != 0 {
            self.main_target_q15 = self.tuning.fade_floor_q15;
            self.aux_silent_periods = 0;
        } else {
            self.aux_silent_periods += 1;
            if self.aux_silent_periods >= AUX_SILENT_RELEASE_PERIODS {
                self.main_target_q15 = 32768;
            }
        }

        // Dry bookkeeping: a source that rendered nothing this period.
        for (s, rendered) in [
            (&mut self.main, main_rendered),
            (&mut self.aux, aux_rendered),
        ] {
            if s.live && rendered == 0 && !s.has(1) {
                s.dry_periods += 1;
                if s.dry_periods >= DRY_RELEASE_PERIODS {
                    s.live = false;
                    s.unprime();
                }
            } else if rendered > 0 {
                s.dry_periods = 0;
            }
        }

        // The software volume, for cards without a usable mixer control;
        // at unity it costs nothing.
        if soft_gain_q15 != 32768 || self.volume_q15 != 32768 {
            self.apply_volume(soft_gain_q15);
        }

        let mut silent = true;
        for (o, &v) in out.iter_mut().zip(self.mix.iter()) {
            *o = v.clamp(-32768, 32767) as i16;
            silent = silent && v <= i32::from(SILENCE_LEVEL) && v >= -i32::from(SILENCE_LEVEL);
        }

        // Release the codec after a real idle stretch: nothing to play, or
        // only digital silence from a stream the phone keeps open. The
        // kernel then powers the DAC and the headphone amplifier down (no
        // idle hiss), the period wakeups stop, and a future owner can
        // reconfigure the hardware. Not while a source fills its buffer:
        // that silence is a pause.
        let filling = (!self.main.live && self.main.has(1)) || (!self.aux.live && self.aux.has(1));
        let nothing =
            main_rendered == 0 && aux_rendered == 0 && !self.main.has(1) && !self.aux.has(1);
        let mut release = false;
        if (silent && !filling) || nothing {
            self.idle_periods += 1;
            release = self.idle_periods * MIX_PERIOD_MS > self.tuning.idle_hold_ms;
        } else {
            self.idle_periods = 0;
        }
        Period { silent, release }
    }

    fn apply_volume(&mut self, target_q15: i32) {
        for frame in self.mix.as_chunks_mut::<MIX_CHANNELS>().0 {
            if self.volume_q15 < target_q15 {
                self.volume_q15 = (self.volume_q15 + VOLUME_STEP_Q15).min(target_q15);
            } else if self.volume_q15 > target_q15 {
                self.volume_q15 = (self.volume_q15 - VOLUME_STEP_Q15).max(target_q15);
            }
            // The mix may exceed 16 bits before the final clamp: a 64-bit
            // product.
            let v = i64::from(self.volume_q15);
            for s in frame {
                *s = ((i64::from(*s) * v) >> 15) as i32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::Format;

    const TUNING: Tuning = Tuning {
        fade_floor_q15: 9830, // 0.3
        call_prefill_segments: 6,
        voice_max_latency_ms: 300,
        idle_hold_ms: 1500,
    };

    struct Rig {
        main: Arc<Queue<Segment>>,
        aux: Arc<Queue<Segment>>,
        e: Engine,
        now: u64,
        out: Vec<i16>,
    }

    impl Rig {
        fn new() -> Self {
            let main = Arc::new(Queue::new(128));
            let aux = Arc::new(Queue::new(24));
            let mut e = Engine::new(main.clone(), aux.clone(), None, TUNING);
            e.on_open(32768);
            Rig {
                main,
                aux,
                e,
                now: 1000,
                out: vec![0; MIX_PERIOD * MIX_CHANNELS],
            }
        }

        /// One period with the clock advanced by a period.
        fn period(&mut self, buffer_ms: u32) -> Period {
            self.period_gain(buffer_ms, 32768)
        }

        fn period_gain(&mut self, buffer_ms: u32, gain: i32) -> Period {
            let p = self.e.period(self.now, buffer_ms, gain, &mut self.out);
            self.now += u64::from(MIX_PERIOD_MS);
            p
        }

        fn peak(&self) -> i16 {
            self.out
                .iter()
                .map(|s| s.saturating_abs())
                .max()
                .unwrap_or(0)
        }

        /// `n` music segments of 100 ms (4800 stereo frames) at `value`.
        fn music(&mut self, n: usize, value: i16) {
            for _ in 0..n {
                self.main.push_drop_oldest(Segment::from_slice(
                    Format::STEREO_48K,
                    &vec![value; 2 * 4800],
                ));
            }
        }

        /// `n` voice segments: 16 kHz mono (a call), 20 ms, at `value`.
        fn call(&mut self, n: usize, value: i16) {
            for _ in 0..n {
                self.aux
                    .push_drop_oldest(Segment::from_slice(Format::MONO_16K, &vec![value; 320]));
            }
        }

        /// `n` voice segments of exactly one period: 48 kHz stereo, 1024
        /// frames.
        fn voice_periods(&mut self, n: usize, value: i16) {
            for _ in 0..n {
                self.aux.push_drop_oldest(Segment::from_slice(
                    Format::STEREO_48K,
                    &vec![value; 2 * MIX_PERIOD],
                ));
            }
        }

        /// `n` navigation segments: 48 kHz stereo, 20 ms.
        fn nav(&mut self, n: usize, value: i16) {
            for _ in 0..n {
                self.aux.push_drop_oldest(Segment::from_slice(
                    Format::STEREO_48K,
                    &vec![value; 2 * 960],
                ));
            }
        }
    }

    #[test]
    fn music_waits_for_its_buffer_then_plays() {
        let mut r = Rig::new();
        r.music(3, 1000); // 300 ms queued
        r.period(500);
        assert!(!r.e.main.live, "300 ms is short of the 500 ms buffer");
        assert_eq!(r.peak(), 0);
        r.music(2, 1000); // 500 ms
        r.period(500);
        assert!(r.e.main.live);
        assert_eq!(r.peak(), 1000);
    }

    #[test]
    fn a_short_clip_plays_after_waiting_as_long_as_the_buffer() {
        let mut r = Rig::new();
        r.music(1, 1000); // 100 ms, never reaches 500
        for _ in 0..24 {
            // the 24th period comes 483 ms after the first
            assert!(!r.period(500).release);
            assert!(!r.e.main.live);
        }
        r.period(500);
        assert!(r.e.main.live, "waited 500 ms");
        assert_eq!(r.peak(), 1000);
    }

    #[test]
    fn a_dry_run_refills_and_counts() {
        let mut r = Rig::new();
        r.e.report(r.now); // the stats window starts here
        r.music(5, 1000);
        r.period(500);
        assert!(r.e.main.live);
        for _ in 0..30 {
            r.period(500);
        }
        assert!(!r.e.main.live, "500 ms played in ~24 periods, then dry");
        assert_eq!(r.e.stats.dry, 1);
        r.music(5, 1000);
        r.period(500);
        assert!(r.e.main.live);
        // The line says so.
        let line = r.e.report(r.now + 5000).expect("5 s later");
        assert!(line.contains("1 dry"), "{line}");
        assert!(line.contains("(buffer 500 ms)"), "{line}");
    }

    #[test]
    fn a_backlog_past_the_slack_is_skipped_to_the_buffer() {
        let mut r = Rig::new();
        r.music(5, 1000);
        r.period(500);
        assert!(r.e.main.live);
        r.music(16, 1000); // 1.6 s more: well past 500 + 1000
        r.period(500);
        assert!(r.e.main.queued_ms() <= 500);
        assert!(r.e.stats.skipped_ms >= 1000, "{}", r.e.stats.skipped_ms);
        // Within the slack nothing is skipped.
        let before = r.e.stats.skipped_ms;
        r.music(9, 1000);
        r.period(500);
        assert_eq!(r.e.stats.skipped_ms, before);
    }

    #[test]
    fn voice_waits_for_segments_and_is_capped() {
        let mut r = Rig::new();
        r.call(5, 2000);
        r.period(500);
        assert!(!r.e.aux.live, "a call waits for call_prefill_segments");
        r.call(1, 2000);
        r.period(500);
        assert!(r.e.aux.live);
        assert_eq!(r.peak(), 2000);
        // Navigation starts after two segments.
        let mut r = Rig::new();
        r.nav(1, 2000);
        r.period(500);
        assert!(!r.e.aux.live);
        r.nav(1, 2000);
        r.period(500);
        assert!(r.e.aux.live);
        // A backlog past voice_max_latency_ms is trimmed to it.
        r.nav(24, 2000);
        r.period(500);
        assert!(r.e.aux.count() * 20 <= 300, "{}", r.e.aux.count());
    }

    #[test]
    fn voice_ducks_the_music_and_releases_after_four_periods() {
        let mut r = Rig::new();
        r.music(25, 10000); // 2.5 s: within the slack of a 1500 ms buffer
        r.period(1500);
        assert!(r.e.main.live);
        assert_eq!(r.e.main_target_q15, 32768);
        // Voice in segments of exactly one period, so the periods count.
        r.voice_periods(12, 3000);
        r.period(1500);
        assert_eq!(
            r.e.main_target_q15, TUNING.fade_floor_q15,
            "audible voice ducks"
        );
        assert_eq!(r.e.main_vol_q15, 32768, "the ramp starts next period");
        r.period(1500);
        // The ramp down is 3 per frame: a period moves 3072.
        assert_eq!(r.e.main_vol_q15, 32768 - 3 * MIX_PERIOD as i32);
        for _ in 0..7 {
            r.period(1500);
        }
        assert_eq!(
            r.e.main_vol_q15, TUNING.fade_floor_q15,
            "at the floor after 8 periods"
        );
        // Voice played in 9 periods so far; it runs out in the 12th.
        for _ in 0..3 {
            r.period(1500);
            assert_eq!(r.e.main_target_q15, TUNING.fade_floor_q15);
        }
        // Three quiet periods keep the duck; the fourth releases it.
        for _ in 0..3 {
            r.period(1500);
            assert_eq!(r.e.main_target_q15, TUNING.fade_floor_q15);
        }
        r.period(1500);
        assert_eq!(r.e.main_target_q15, 32768);
        // Back up at 1 per frame.
        r.period(1500);
        assert_eq!(r.e.main_vol_q15, TUNING.fade_floor_q15 + MIX_PERIOD as i32);
        // Digital silence on an open voice stream does not duck.
        r.voice_periods(6, 0);
        for _ in 0..6 {
            r.period(1500);
            assert_eq!(r.e.main_target_q15, 32768);
        }
    }

    #[test]
    fn the_buffer_is_followed_live() {
        let mut r = Rig::new();
        r.music(10, 1000); // 1000 ms queued
        r.period(500);
        assert!(r.e.main.live);
        // Deeper: pauses once to fill.
        r.period(1000);
        assert!(!r.e.main.live);
        r.music(3, 1000);
        r.period(1000);
        assert!(r.e.main.live);
        // Shallower: drops the excess at once.
        let before = r.e.main.queued_ms();
        assert!(before > 600);
        r.period(300);
        assert!(r.e.main.live);
        assert!(
            r.e.main.queued_ms() <= 300 + 100,
            "{}",
            r.e.main.queued_ms()
        );
        // Nothing dropped while not live: the next start fills to the
        // new depth.
        let mut r = Rig::new();
        r.music(2, 1000);
        r.period(500);
        r.period(100);
        assert!(r.e.main.queued_ms() > 150, "{}", r.e.main.queued_ms());
    }

    #[test]
    fn silence_releases_the_device_after_the_hold() {
        let mut r = Rig::new();
        r.music(5, 1000);
        r.period(500);
        let mut periods = 0;
        loop {
            let p = r.period(500);
            periods += 1;
            if p.release {
                break;
            }
            assert!(periods < 200, "never released");
        }
        // ~24 periods of music then 1500 ms / 21 = 72 periods of nothing.
        assert!((90..=100).contains(&periods), "{periods}");
        r.e.on_close();
        assert!(!r.e.main.live);
        // Digital silence the phone keeps sending is idle too; a source
        // filling its buffer is not.
        let mut r = Rig::new();
        r.music(30, 0);
        r.period(500);
        assert!(r.e.main.live);
        let mut periods = 0;
        while !r.period(500).release {
            periods += 1;
            assert!(periods < 200);
        }
        assert!(periods <= 73, "{periods}");
        let mut r = Rig::new();
        r.music(1, 1000);
        for _ in 0..80 {
            assert!(!r.period(1500).release, "a filling source is a pause");
        }
    }

    #[test]
    fn drop_silence_clears_quiet_segments_with_the_device_closed() {
        let mut r = Rig::new();
        r.music(2, 0);
        r.call(1, 0);
        assert!(!r.e.drop_silence());
        assert_eq!(r.e.main.count() + r.e.aux.count(), 0);
        r.music(1, 0);
        r.music(1, 5);
        assert!(r.e.drop_silence());
        assert_eq!(r.e.main.count(), 1);
    }

    #[test]
    fn the_software_volume_ramps_sixty_four_per_frame() {
        let mut r = Rig::new();
        r.e.on_open(32768);
        r.music(30, 16000);
        r.period_gain(500, 32768);
        assert!(r.e.main.live);
        r.period_gain(500, 0);
        // 32768 / 64 = 512 frames to reach silence: the first half of the
        // period fades, the second is silent.
        assert_eq!(r.e.volume_q15, 0);
        assert!(r.out[0].abs() > 15000 && r.out[0].abs() < 16000);
        assert_eq!(r.out[2 * 600], 0);
        assert_eq!(r.out[2 * 1023], 0);
        r.period_gain(500, 4096);
        assert_eq!(r.e.volume_q15, 4096);
        assert_eq!(r.out[2 * 1023], 2000);
        // A reopen starts at the level, no ramp.
        r.e.on_open(4096);
        assert_eq!(r.e.volume_q15, 4096);
    }

    #[test]
    fn the_sum_clamps() {
        let mut r = Rig::new();
        r.music(30, 30000);
        r.period(500);
        r.nav(2, 30000);
        r.period(500);
        r.period(500);
        assert_eq!(r.peak(), 32767);
    }
}
