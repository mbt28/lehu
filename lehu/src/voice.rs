// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The map's spoken prompts on the board: what `lehu_map::Voice` decides
//! to say, put together from the clips on the SD card (map.voice-dir, as
//! scripts/voice-clips.py makes them) and handed to the mixer as one
//! phrase.
//!
//! A clip is read the first time a prompt needs it and kept while the map
//! is open: a few dozen KB each, plain 16-bit PCM in the shape the mixer
//! takes, so the board only copies samples. A clip that is missing or
//! unreadable is said once in the log, and the prompts that need it stay
//! silent.

use std::collections::HashMap;
use std::path::Path;

use lehu_audio::{Format, Segment, wav};
use lehu_map::{Ahead, Clip, Turn, Voice};

use crate::audio::Audio;

/// The pause between two clips of a phrase, on top of the little silence
/// each clip keeps at its ends: a comma's.
const GAP_MS: u32 = 70;
/// A clip is a few words: a file larger than this is not one.
const MAX_CLIP_BYTES: u64 = 512 * 1024;

/// The prompts along a drive and the clips they are said with.
pub struct Speaker {
    dir: String,
    voice: Voice,
    /// The clips read so far, by name; None: missing or unreadable.
    clips: HashMap<String, Option<(Format, Vec<i16>)>>,
    /// The log has said why the prompts are silent.
    warned: bool,
}

impl Speaker {
    pub fn new(dir: &str) -> Self {
        Self {
            dir: dir.to_string(),
            voice: Voice::new(),
            clips: HashMap::new(),
            warned: false,
        }
    }

    /// The directory the clips come from.
    pub fn dir(&self) -> &str {
        &self.dir
    }

    /// With each new position: the prompt due, if any, to the mixer.
    pub fn tick(&mut self, ahead: &Ahead<'_>, kmh: u32, then: Option<(Turn, u32)>, audio: &Audio) {
        if let Some(clips) = self.voice.tick(ahead, kmh, then) {
            self.say(&clips, audio);
        }
    }

    /// A phrase of these clips, to the mixer now (silent when a clip is
    /// missing).
    pub fn say(&mut self, clips: &[Clip], audio: &Audio) {
        if let Some(segment) = self.phrase(clips) {
            log::info!(
                "map: says {} ({} ms)",
                clips.iter().map(|c| c.name()).collect::<Vec<_>>().join(" "),
                segment.duration_ms()
            );
            audio.prompt(segment);
        }
    }

    /// The map closed: the clips go, read again when it opens.
    pub fn release(&mut self) {
        self.clips = HashMap::new();
    }

    /// The clips one after the other, a short pause between them, as one
    /// segment. None when one is missing, or they are not all one shape.
    fn phrase(&mut self, clips: &[Clip]) -> Option<Segment> {
        let names: Vec<String> = clips.iter().map(|c| c.name()).collect();
        for name in &names {
            if !self.clips.contains_key(name) {
                let clip = self.read(name);
                self.clips.insert(name.clone(), clip);
            }
        }
        let parts: Vec<&(Format, Vec<i16>)> = names
            .iter()
            .map(|n| self.clips.get(n).and_then(Option::as_ref))
            .collect::<Option<_>>()?;
        let format = parts.first()?.0;
        if parts.iter().any(|(f, _)| *f != format) {
            if !self.warned {
                self.warned = true;
                log::warn!(
                    "map voice: the clips in {} differ in rate or channels; make them again",
                    self.dir
                );
            }
            return None;
        }
        let gap = (format.rate * GAP_MS / 1000) as usize * usize::from(format.channels);
        let total = parts.iter().map(|(_, pcm)| pcm.len()).sum::<usize>() + gap * (parts.len() - 1);
        let mut segment = Segment::with_capacity(total);
        segment.format = format;
        for (i, (_, pcm)) in parts.iter().enumerate() {
            if i > 0 {
                segment.space(gap)?.fill(0);
            }
            segment.write(pcm);
        }
        Some(segment)
    }

    /// A clip's samples from its file.
    fn read(&mut self, name: &str) -> Option<(Format, Vec<i16>)> {
        let path = Path::new(&self.dir).join(format!("{name}.wav"));
        match read_clip(&path) {
            Ok(clip) => Some(clip),
            Err(e) => {
                self.warn(format_args!(
                    "{}: {e}; prompts that need it stay silent (scripts/voice-clips.py makes the clips)",
                    path.display()
                ));
                None
            }
        }
    }

    /// Why a prompt is silent, said once.
    fn warn(&mut self, why: std::fmt::Arguments<'_>) {
        if !self.warned {
            self.warned = true;
            log::warn!("map voice: {why}");
        }
    }
}

fn read_clip(path: &Path) -> Result<(Format, Vec<i16>), String> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size > MAX_CLIP_BYTES {
        return Err(format!("{size} bytes, too long for a clip"));
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    wav::read(&bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn wav_bytes(rate: u32, samples: &[i16]) -> Vec<u8> {
        let data = samples.len() as u32 * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data.to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lehu-voice-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_phrase_is_its_clips_with_a_pause_between() {
        let d = dir("phrase");
        std::fs::write(d.join("in-500.wav"), wav_bytes(24000, &[1, 2, 3])).unwrap();
        std::fs::write(d.join("turn-left.wav"), wav_bytes(24000, &[4, 5])).unwrap();
        let mut s = Speaker::new(d.to_str().unwrap());
        let seg = s
            .phrase(&[Clip::In(500), Clip::Do(Turn::Left)])
            .expect("both clips are there");
        assert_eq!(seg.format, Format::new(24000, 1));
        let gap = (24000 * GAP_MS / 1000) as usize;
        assert_eq!(seg.len(), 3 + gap + 2);
        assert_eq!(&seg.data()[..3], &[1, 2, 3]);
        assert!(seg.data()[3..3 + gap].iter().all(|&v| v == 0));
        assert_eq!(&seg.data()[3 + gap..], &[4, 5]);
        // Kept: the files may go, the clips stay until the map closes.
        std::fs::remove_file(d.join("in-500.wav")).unwrap();
        assert!(s.phrase(&[Clip::In(500)]).is_some());
        s.release();
        assert!(s.phrase(&[Clip::In(500)]).is_none());
    }

    #[test]
    fn a_missing_or_odd_clip_keeps_the_prompt_silent() {
        let d = dir("missing");
        std::fs::write(d.join("now.wav"), wav_bytes(24000, &[1])).unwrap();
        std::fs::write(d.join("turn-right.wav"), wav_bytes(22050, &[1])).unwrap();
        std::fs::write(d.join("then.wav"), b"not a wav").unwrap();
        let mut s = Speaker::new(d.to_str().unwrap());
        assert!(s.phrase(&[Clip::Now, Clip::Do(Turn::Left)]).is_none());
        assert!(s.warned, "said in the log");
        assert!(
            s.phrase(&[Clip::Now, Clip::Do(Turn::Right)]).is_none(),
            "two rates"
        );
        assert!(s.phrase(&[Clip::Then]).is_none());
        assert!(s.phrase(&[Clip::Now]).is_some());
    }
}
