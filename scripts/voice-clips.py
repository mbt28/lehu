#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 Mehmet Tekbas
"""Make the map's spoken prompts, on a PC: one WAV clip a phrase fragment.

    scripts/voice-clips.py VOICE.onnx OUT_DIR [--lang LANG] [--rate 24000] [--demo FILE]

LEHU says a turn by playing clips one after the other ("In 500 metres" +
"turn left", "Now" + "turn left", "Take the second exit"); this writes
them into OUT_DIR, each <name>.wav, the names the board asks for
(crates/lehu-map/src/voice.rs; a test there checks this list), in the
voice's language: English, Hungarian, German, French, Spanish, Italian or
Turkish (--lang, else the language its file name starts with: hu_HU-...).
tools/lehu-maps makes them and writes them to the card's voice/<voice>.

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
# whose test reads them here), and what each says, in each language a
# voice can speak. The board plays them one after the other ("in-500" then
# "turn-left"), so in each language a distance and a turn read as one
# sentence; a clip that goes on a sentence begun by another starts small.
PHRASES = {
    "en": {
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
    },
    "hu": {
        "in-50": "Ötven méter múlva.",
        "in-100": "Száz méter múlva.",
        "in-150": "Százötven méter múlva.",
        "in-200": "Kétszáz méter múlva.",
        "in-250": "Kétszázötven méter múlva.",
        "in-300": "Háromszáz méter múlva.",
        "in-400": "Négyszáz méter múlva.",
        "in-500": "Ötszáz méter múlva.",
        "in-600": "Hatszáz méter múlva.",
        "in-700": "Hétszáz méter múlva.",
        "in-800": "Nyolcszáz méter múlva.",
        "in-900": "Kilencszáz méter múlva.",
        "in-1000": "Egy kilométer múlva.",
        "in-1500": "Másfél kilométer múlva.",
        "in-2000": "Két kilométer múlva.",
        "straight": "Haladjon egyenesen tovább.",
        "bear-left": "Tartson balra.",
        "turn-left": "Forduljon balra.",
        "sharp-left": "Forduljon élesen balra.",
        "bear-right": "Tartson jobbra.",
        "turn-right": "Forduljon jobbra.",
        "sharp-right": "Forduljon élesen jobbra.",
        "u-turn": "Forduljon vissza.",
        "roundabout-1": "A körforgalomban hajtson ki az első kijáraton.",
        "roundabout-2": "A körforgalomban hajtson ki a második kijáraton.",
        "roundabout-3": "A körforgalomban hajtson ki a harmadik kijáraton.",
        "roundabout-4": "A körforgalomban hajtson ki a negyedik kijáraton.",
        "roundabout-5": "A körforgalomban hajtson ki az ötödik kijáraton.",
        "roundabout-6": "A körforgalomban hajtson ki a hatodik kijáraton.",
        "roundabout": "Hajtson be a körforgalomba.",
        "exit-1": "Hajtson ki az első kijáraton.",
        "exit-2": "Hajtson ki a második kijáraton.",
        "exit-3": "Hajtson ki a harmadik kijáraton.",
        "exit-4": "Hajtson ki a negyedik kijáraton.",
        "exit-5": "Hajtson ki az ötödik kijáraton.",
        "exit-6": "Hajtson ki a hatodik kijáraton.",
        "arrive-ahead": "Megérkezik az úti céljához.",
        "arrived": "Megérkezett az úti céljához.",
        "now": "Most",
        "then": "majd",
        "speed-camera": "Figyelem, traffipax.",
    },
    "de": {
        "in-50": "In fünfzig Metern",
        "in-100": "In hundert Metern",
        "in-150": "In hundertfünfzig Metern",
        "in-200": "In zweihundert Metern",
        "in-250": "In zweihundertfünfzig Metern",
        "in-300": "In dreihundert Metern",
        "in-400": "In vierhundert Metern",
        "in-500": "In fünfhundert Metern",
        "in-600": "In sechshundert Metern",
        "in-700": "In siebenhundert Metern",
        "in-800": "In achthundert Metern",
        "in-900": "In neunhundert Metern",
        "in-1000": "In einem Kilometer",
        "in-1500": "In anderthalb Kilometern",
        "in-2000": "In zwei Kilometern",
        "straight": "geradeaus weiterfahren.",
        "bear-left": "links halten.",
        "turn-left": "links abbiegen.",
        "sharp-left": "scharf links abbiegen.",
        "bear-right": "rechts halten.",
        "turn-right": "rechts abbiegen.",
        "sharp-right": "scharf rechts abbiegen.",
        "u-turn": "bitte wenden.",
        "roundabout-1": "im Kreisverkehr die erste Ausfahrt nehmen.",
        "roundabout-2": "im Kreisverkehr die zweite Ausfahrt nehmen.",
        "roundabout-3": "im Kreisverkehr die dritte Ausfahrt nehmen.",
        "roundabout-4": "im Kreisverkehr die vierte Ausfahrt nehmen.",
        "roundabout-5": "im Kreisverkehr die fünfte Ausfahrt nehmen.",
        "roundabout-6": "im Kreisverkehr die sechste Ausfahrt nehmen.",
        "roundabout": "in den Kreisverkehr einfahren.",
        "exit-1": "Die erste Ausfahrt nehmen.",
        "exit-2": "Die zweite Ausfahrt nehmen.",
        "exit-3": "Die dritte Ausfahrt nehmen.",
        "exit-4": "Die vierte Ausfahrt nehmen.",
        "exit-5": "Die fünfte Ausfahrt nehmen.",
        "exit-6": "Die sechste Ausfahrt nehmen.",
        "arrive-ahead": "erreichen Sie Ihr Ziel.",
        "arrived": "Sie haben Ihr Ziel erreicht.",
        "now": "Jetzt",
        "then": "dann",
        "speed-camera": "Achtung, Radarkontrolle.",
    },
    "fr": {
        "in-50": "Dans cinquante mètres,",
        "in-100": "Dans cent mètres,",
        "in-150": "Dans cent cinquante mètres,",
        "in-200": "Dans deux cents mètres,",
        "in-250": "Dans deux cent cinquante mètres,",
        "in-300": "Dans trois cents mètres,",
        "in-400": "Dans quatre cents mètres,",
        "in-500": "Dans cinq cents mètres,",
        "in-600": "Dans six cents mètres,",
        "in-700": "Dans sept cents mètres,",
        "in-800": "Dans huit cents mètres,",
        "in-900": "Dans neuf cents mètres,",
        "in-1000": "Dans un kilomètre,",
        "in-1500": "Dans un kilomètre et demi,",
        "in-2000": "Dans deux kilomètres,",
        "straight": "continuez tout droit.",
        "bear-left": "restez à gauche.",
        "turn-left": "tournez à gauche.",
        "sharp-left": "tournez franchement à gauche.",
        "bear-right": "restez à droite.",
        "turn-right": "tournez à droite.",
        "sharp-right": "tournez franchement à droite.",
        "u-turn": "faites demi-tour.",
        "roundabout-1": "au rond-point, prenez la première sortie.",
        "roundabout-2": "au rond-point, prenez la deuxième sortie.",
        "roundabout-3": "au rond-point, prenez la troisième sortie.",
        "roundabout-4": "au rond-point, prenez la quatrième sortie.",
        "roundabout-5": "au rond-point, prenez la cinquième sortie.",
        "roundabout-6": "au rond-point, prenez la sixième sortie.",
        "roundabout": "entrez dans le rond-point.",
        "exit-1": "Prenez la première sortie.",
        "exit-2": "Prenez la deuxième sortie.",
        "exit-3": "Prenez la troisième sortie.",
        "exit-4": "Prenez la quatrième sortie.",
        "exit-5": "Prenez la cinquième sortie.",
        "exit-6": "Prenez la sixième sortie.",
        "arrive-ahead": "vous arriverez à destination.",
        "arrived": "Vous êtes arrivé à destination.",
        "now": "Maintenant,",
        "then": "puis",
        "speed-camera": "Attention, radar.",
    },
    "es": {
        "in-50": "En cincuenta metros,",
        "in-100": "En cien metros,",
        "in-150": "En ciento cincuenta metros,",
        "in-200": "En doscientos metros,",
        "in-250": "En doscientos cincuenta metros,",
        "in-300": "En trescientos metros,",
        "in-400": "En cuatrocientos metros,",
        "in-500": "En quinientos metros,",
        "in-600": "En seiscientos metros,",
        "in-700": "En setecientos metros,",
        "in-800": "En ochocientos metros,",
        "in-900": "En novecientos metros,",
        "in-1000": "En un kilómetro,",
        "in-1500": "En un kilómetro y medio,",
        "in-2000": "En dos kilómetros,",
        "straight": "continúe recto.",
        "bear-left": "manténgase a la izquierda.",
        "turn-left": "gire a la izquierda.",
        "sharp-left": "gire bruscamente a la izquierda.",
        "bear-right": "manténgase a la derecha.",
        "turn-right": "gire a la derecha.",
        "sharp-right": "gire bruscamente a la derecha.",
        "u-turn": "cambie de sentido.",
        "roundabout-1": "en la rotonda, tome la primera salida.",
        "roundabout-2": "en la rotonda, tome la segunda salida.",
        "roundabout-3": "en la rotonda, tome la tercera salida.",
        "roundabout-4": "en la rotonda, tome la cuarta salida.",
        "roundabout-5": "en la rotonda, tome la quinta salida.",
        "roundabout-6": "en la rotonda, tome la sexta salida.",
        "roundabout": "entre en la rotonda.",
        "exit-1": "Tome la primera salida.",
        "exit-2": "Tome la segunda salida.",
        "exit-3": "Tome la tercera salida.",
        "exit-4": "Tome la cuarta salida.",
        "exit-5": "Tome la quinta salida.",
        "exit-6": "Tome la sexta salida.",
        "arrive-ahead": "llegará a su destino.",
        "arrived": "Ha llegado a su destino.",
        "now": "Ahora,",
        "then": "después",
        "speed-camera": "Atención, radar.",
    },
    "it": {
        "in-50": "Tra cinquanta metri,",
        "in-100": "Tra cento metri,",
        "in-150": "Tra centocinquanta metri,",
        "in-200": "Tra duecento metri,",
        "in-250": "Tra duecentocinquanta metri,",
        "in-300": "Tra trecento metri,",
        "in-400": "Tra quattrocento metri,",
        "in-500": "Tra cinquecento metri,",
        "in-600": "Tra seicento metri,",
        "in-700": "Tra settecento metri,",
        "in-800": "Tra ottocento metri,",
        "in-900": "Tra novecento metri,",
        "in-1000": "Tra un chilometro,",
        "in-1500": "Tra un chilometro e mezzo,",
        "in-2000": "Tra due chilometri,",
        "straight": "prosegui dritto.",
        "bear-left": "mantieni la sinistra.",
        "turn-left": "svolta a sinistra.",
        "sharp-left": "svolta decisamente a sinistra.",
        "bear-right": "mantieni la destra.",
        "turn-right": "svolta a destra.",
        "sharp-right": "svolta decisamente a destra.",
        "u-turn": "fai inversione.",
        "roundabout-1": "alla rotonda, prendi la prima uscita.",
        "roundabout-2": "alla rotonda, prendi la seconda uscita.",
        "roundabout-3": "alla rotonda, prendi la terza uscita.",
        "roundabout-4": "alla rotonda, prendi la quarta uscita.",
        "roundabout-5": "alla rotonda, prendi la quinta uscita.",
        "roundabout-6": "alla rotonda, prendi la sesta uscita.",
        "roundabout": "entra nella rotonda.",
        "exit-1": "Prendi la prima uscita.",
        "exit-2": "Prendi la seconda uscita.",
        "exit-3": "Prendi la terza uscita.",
        "exit-4": "Prendi la quarta uscita.",
        "exit-5": "Prendi la quinta uscita.",
        "exit-6": "Prendi la sesta uscita.",
        "arrive-ahead": "arriverai a destinazione.",
        "arrived": "Sei arrivato a destinazione.",
        "now": "Ora,",
        "then": "poi",
        "speed-camera": "Attenzione, autovelox.",
    },
    "tr": {
        "in-50": "Elli metre sonra",
        "in-100": "Yüz metre sonra",
        "in-150": "Yüz elli metre sonra",
        "in-200": "İki yüz metre sonra",
        "in-250": "İki yüz elli metre sonra",
        "in-300": "Üç yüz metre sonra",
        "in-400": "Dört yüz metre sonra",
        "in-500": "Beş yüz metre sonra",
        "in-600": "Altı yüz metre sonra",
        "in-700": "Yedi yüz metre sonra",
        "in-800": "Sekiz yüz metre sonra",
        "in-900": "Dokuz yüz metre sonra",
        "in-1000": "Bir kilometre sonra",
        "in-1500": "Bir buçuk kilometre sonra",
        "in-2000": "İki kilometre sonra",
        "straight": "düz devam edin.",
        "bear-left": "hafif sola dönün.",
        "turn-left": "sola dönün.",
        "sharp-left": "keskin sola dönün.",
        "bear-right": "hafif sağa dönün.",
        "turn-right": "sağa dönün.",
        "sharp-right": "keskin sağa dönün.",
        "u-turn": "U dönüşü yapın.",
        "roundabout-1": "döner kavşakta birinci çıkıştan çıkın.",
        "roundabout-2": "döner kavşakta ikinci çıkıştan çıkın.",
        "roundabout-3": "döner kavşakta üçüncü çıkıştan çıkın.",
        "roundabout-4": "döner kavşakta dördüncü çıkıştan çıkın.",
        "roundabout-5": "döner kavşakta beşinci çıkıştan çıkın.",
        "roundabout-6": "döner kavşakta altıncı çıkıştan çıkın.",
        "roundabout": "döner kavşağa girin.",
        "exit-1": "Birinci çıkıştan çıkın.",
        "exit-2": "İkinci çıkıştan çıkın.",
        "exit-3": "Üçüncü çıkıştan çıkın.",
        "exit-4": "Dördüncü çıkıştan çıkın.",
        "exit-5": "Beşinci çıkıştan çıkın.",
        "exit-6": "Altıncı çıkıştan çıkın.",
        "arrive-ahead": "varış noktanıza ulaşacaksınız.",
        "arrived": "Varış noktanıza ulaştınız.",
        "now": "Şimdi",
        "then": "sonra",
        "speed-camera": "Dikkat, hız kamerası.",
    },
}

# The languages, by their code (a Piper voice's name starts with it).
LANGUAGES = {
    "en": "English",
    "hu": "Hungarian",
    "de": "German",
    "fr": "French",
    "es": "Spanish",
    "it": "Italian",
    "tr": "Turkish",
}

# Every language has every clip.
for _lang, _clips in PHRASES.items():
    assert list(_clips) == list(PHRASES["en"]), _lang
CLIPS = PHRASES["en"]

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
    p.add_argument("--lang", choices=sorted(PHRASES),
                   help="the phrases' language (default: the voice's, from its file name)")
    p.add_argument("--rate", type=int, default=24000, help="clip rate, Hz (default 24000)")
    p.add_argument("--length-scale", type=float, default=None,
                   help="slower (above 1) or faster speech than the voice's own")
    p.add_argument("--demo", help="also write whole prompts to this WAV file")
    args = p.parse_args()
    if not 8000 <= args.rate <= 48000:
        sys.exit("--rate is 8000..48000")

    lang = args.lang or os.path.basename(args.voice).split("_")[0]
    if lang not in PHRASES:
        sys.exit(f"no phrases in {lang!r}: --lang one of {', '.join(sorted(PHRASES))}")
    clips = PHRASES[lang]
    voice = PiperVoice.load(args.voice)
    config = SynthesisConfig(length_scale=args.length_scale)
    os.makedirs(args.out, exist_ok=True)
    made = {}
    total = 0
    for name, text in clips.items():
        samples = clip(voice, config, text, args.rate)
        path = os.path.join(args.out, f"{name}.wav")
        write(path, samples, args.rate)
        made[name] = samples
        total += os.path.getsize(path)
        print(f"{name:14} {len(samples) * 1000 // args.rate:5} ms  {text}")
    print(f"{len(made)} clips in {LANGUAGES[lang]}, {total // 1024} KB, in {args.out}")

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
