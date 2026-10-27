#!/usr/bin/env python3
"""Writes Keystone's three event sounds, synthesized here from sine partials with no source
material. The script's output is dedicated to the public domain under CC0 1.0.

`python3 scripts/make-sounds.py` rewrites crates/keystone-desktop/sounds/*.wav: 44.1 kHz mono
16-bit WAV, each under half a second, peaking at -12 dBFS. The output is deterministic.
"""
import math
import struct
import wave
from pathlib import Path

RATE = 44100
PEAK = 10 ** (-12 / 20)
OUT = Path(__file__).resolve().parent.parent / 'crates/keystone-desktop/sounds'

# A soft bell: the fundamental, a quiet octave and a faint inharmonic glass partial.
PARTIALS = [(1.0, 1.0, 1.0), (2.0, 0.18, 0.6), (2.76, 0.05, 0.35)]


def note(samples, start, freq, gain, decay):
    """Adds a struck note at `start` seconds that decays with time constant `decay`."""
    begin = int(start * RATE)
    attack = 0.004
    for i in range(begin, len(samples)):
        t = (i - begin) / RATE
        envelope = (0.5 - 0.5 * math.cos(math.pi * t / attack)) if t < attack else 1.0
        value = 0.0
        for ratio, level, speed in PARTIALS:
            value += level * math.exp(-t / (decay * speed)) * math.sin(2 * math.pi * freq * ratio * t)
        samples[i] += gain * envelope * value


def render(name, length, notes):
    samples = [0.0] * int(length * RATE)
    for start, freq, gain, decay in notes:
        note(samples, start, freq, gain, decay)
    fade = int(0.03 * RATE)
    for i in range(fade):
        samples[-1 - i] *= i / fade
    scale = PEAK / max(abs(s) for s in samples)
    frames = b''.join(struct.pack('<h', round(s * scale * 32767)) for s in samples)
    with wave.open(str(OUT / f'{name}.wav'), 'wb') as out:
        out.setnchannels(1)
        out.setsampwidth(2)
        out.setframerate(RATE)
        out.writeframes(frames)


OUT.mkdir(parents=True, exist_ok=True)
# Done: a rising fourth, G5 to C6.
render('turn-finished', 0.42, [(0.0, 783.99, 0.8, 0.09), (0.085, 1046.5, 1.0, 0.13)])
# Needs approval: two even taps on A5, asking to be looked at.
render('approval', 0.40, [(0.0, 880.0, 1.0, 0.07), (0.14, 880.0, 0.9, 0.10)])
# Failed: a falling minor third, E5 to C#5, lower and duller.
render('turn-failed', 0.45, [(0.0, 659.26, 1.0, 0.08), (0.11, 554.37, 0.9, 0.14)])
