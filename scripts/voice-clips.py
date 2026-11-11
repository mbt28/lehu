#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Mehmet Tekbas
"""Make the map's spoken prompts, on a PC: one WAV clip a phrase fragment.

    scripts/voice-clips.py VOICE.onnx OUT_DIR [--rate 24000] [--demo FILE]

LEHU says a turn by playing clips one after the other ("In 500 metres" +
"turn left", "Now" + "turn left", "Take the second exit"); this writes
them into OUT_DIR, each <name>.wav, the names the board asks for
(crates/lehu-map/src/voice.rs; a test there checks this list). Copy
OUT_DIR to the board's map.voice-dir (/var/lib/lehu/voice/en).

The speech is Piper's (https://github.com/OHF-voice/piper1-gpl), offline,
from a voice of its catalogue; en_GB-cori-high is public domain (its
recordings are LibriVox's; the model was trained from scratch on them):

    python3 -m venv voice-venv
    voice-venv/bin/pip install piper-tts
    voice-venv/bin/python -m piper.download_voices --data-dir voices en_GB-cori-high
    voice-venv/bin/python scripts/voice-clips.py voices/en_GB-cori-high.onnx voice/en

The clips are made so the board only copies them: 16-bit PCM, mono, at
--rate (24 kHz, half the mixer's 48, so its resampler steps evenly), the
silence at their ends trimmed to a little, all at one loudness. --demo
also writes a few whole prompts, put together as the board does, to hear
on the PC.
"""

import argparse
import os
import sys
import wave

import numpy as np
from piper import PiperVoice, SynthesisConfig

# The clips, by the name the board asks for (crates/lehu-map/src/voice.rs,
# whose test reads them here), and what each says.
CLIPS = {
    "in-50": "In fifty metres.",
    "in-100": "In one hundred metres.",
    "in-150": "In one hundred and fifty metres.",
    "in-200": "In two hundred metres.",
    "in-250": "In two hundred and fifty metres.",
    "in-300": "In three hundred metres.",
    "in-400": "In four hundred metres.",
    "in-500": "In five hundred metres.",
    "in-600": "In six hundred metres.",
    "in-700": "In seven hundred metres.",
    "in-800": "In eight hundred metres.",
    "in-900": "In nine hundred metres.",
    "in-1000": "In one kilometre.",
    "in-1500": "In one and a half kilometres.",
    "in-2000": "In two kilometres.",
    "straight": "Continue straight on.",
    "bear-left": "Bear left.",
    "turn-left": "Turn left.",
    "sharp-left": "Turn sharp left.",
    "bear-right": "Bear right.",
    "turn-right": "Turn right.",
    "sharp-right": "Turn sharp right.",
    "u-turn": "Make a U-turn.",
    "roundabout-1": "At the roundabout, take the first exit.",
    "roundabout-2": "At the roundabout, take the second exit.",
    "roundabout-3": "At the roundabout, take the third exit.",
    "roundabout-4": "At the roundabout, take the fourth exit.",
    "roundabout-5": "At the roundabout, take the fifth exit.",
    "roundabout-6": "At the roundabout, take the sixth exit.",
    "roundabout": "Enter the roundabout.",
    "exit-1": "Take the first exit.",
    "exit-2": "Take the second exit.",
    "exit-3": "Take the third exit.",
    "exit-4": "Take the fourth exit.",
    "exit-5": "Take the fifth exit.",
    "exit-6": "Take the sixth exit.",
    "arrive-ahead": "You will reach your destination.",
    "arrived": "You have arrived at your destination.",
    "now": "Now,",
    "then": "then",
    "speed-camera": "Speed camera ahead.",
}

# What the board puts between two clips (lehu/src/voice.rs GAP_MS).
GAP_MS = 70
# Silence kept at each end of a clip, and the level below which the ends
# count as silence (of the clip's peak).
PAD_MS = 25
QUIET = 0.02
# Every clip's peak: a little under full scale, the same for all.
PEAK = 0.7

DEMO = [
    ["in-500", "turn-left"],
    ["in-200", "turn-left", "then", "turn-right"],
    ["now", "turn-left"],
    ["in-1000", "roundabout-2"],
    ["exit-2"],
    ["in-200", "arrive-ahead"],
    ["arrived"],
    ["speed-camera"],
]


def resample(x, rate_in, rate_out):
    """x at rate_out, through its spectrum: the clip is short, and silent
    at both ends, so treating it as periodic costs nothing."""
    if rate_in == rate_out:
        return x
    n = len(x)
    m = int(round(n * rate_out / rate_in))
    spectrum = np.fft.rfft(x)
    out = np.zeros(m // 2 + 1, dtype=complex)
    k = min(len(spectrum), len(out))
    out[:k] = spectrum[:k]
    return np.fft.irfft(out, m) * (m / n)


def trim(x, rate):
    """The clip without its silent ends, but PAD_MS of them."""
    loud = np.flatnonzero(np.abs(x) > QUIET * np.max(np.abs(x)))
    if len(loud) == 0:
        return x
    pad = rate * PAD_MS // 1000
    return x[max(0, loud[0] - pad):loud[-1] + pad]


def clip(voice, config, text, rate):
    audio = []
    source_rate = None
    for chunk in voice.synthesize(text, syn_config=config):
        audio.append(chunk.audio_int16_array)
        source_rate = chunk.sample_rate
    x = np.concatenate(audio).astype(np.float64) / 32768.0
    x = trim(x, source_rate)
    x = resample(x, source_rate, rate)
    x *= PEAK / max(np.max(np.abs(x)), 1e-6)
    # A 5 ms fade at both ends: no click where it is cut.
    fade = min(len(x) // 2, rate // 200)
    ramp = np.linspace(0.0, 1.0, fade)
    x[:fade] *= ramp
    x[len(x) - fade:] *= ramp[::-1]
    return np.clip(np.round(x * 32767), -32768, 32767).astype("<i2")


def write(path, samples, rate):
    with wave.open(path, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(samples.tobytes())


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("voice", help="a Piper voice, its .onnx (the .onnx.json next to it)")
    p.add_argument("out", help="the directory the clips go in")
    p.add_argument("--rate", type=int, default=24000, help="clip rate, Hz (default 24000)")
    p.add_argument("--length-scale", type=float, default=None,
                   help="slower (above 1) or faster speech than the voice's own")
    p.add_argument("--demo", help="also write whole prompts to this WAV file")
    args = p.parse_args()
    if not 8000 <= args.rate <= 48000:
        sys.exit("--rate is 8000..48000")

    voice = PiperVoice.load(args.voice)
    config = SynthesisConfig(length_scale=args.length_scale)
    os.makedirs(args.out, exist_ok=True)
    made = {}
    total = 0
    for name, text in CLIPS.items():
        samples = clip(voice, config, text, args.rate)
        path = os.path.join(args.out, f"{name}.wav")
        write(path, samples, args.rate)
        made[name] = samples
        total += os.path.getsize(path)
        print(f"{name:14} {len(samples) * 1000 // args.rate:5} ms  {text}")
    print(f"{len(made)} clips, {total // 1024} KB, in {args.out}")

    if args.demo:
        gap = np.zeros(args.rate * GAP_MS // 1000, dtype="<i2")
        second = np.zeros(args.rate, dtype="<i2")
        parts = []
        for phrase in DEMO:
            for i, name in enumerate(phrase):
                if i:
                    parts.append(gap)
                parts.append(made[name])
            parts.append(second)
        write(args.demo, np.concatenate(parts), args.rate)
        print(f"the demo prompts in {args.demo}")


if __name__ == "__main__":
    main()
