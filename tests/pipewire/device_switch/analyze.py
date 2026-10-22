#!/usr/bin/env python3
"""Reads work/recA.wav / recB.wav written by run.sh and prints which profile each device carried."""
import struct, sys
from pathlib import Path
import numpy as np

W = Path(__file__).resolve().parent / "work"


def read(fn):
    d = Path(fn).read_bytes(); i = 12
    while i < len(d):
        cid, sz = d[i:i + 4], struct.unpack("<I", d[i + 4:i + 8])[0]
        if cid == b"fmt ": fmt = struct.unpack("<HHIIHH", d[i + 8:i + 24])
        if cid == b"data": return np.frombuffer(d[i + 8:i + 8 + sz], dtype="<f4").reshape(-1, fmt[1]), fmt[2]
        i += 8 + sz + (sz & 1)


def amp(x, f, fs):
    t = np.arange(len(x)) / fs
    return 2 * abs(np.sum(x * np.exp(-2j * np.pi * f * t))) / len(x)


a, fs = read(W / "recA.wav"); b, _ = read(W / "recB.wav")
t0 = min(np.where(np.abs(r).max(axis=1) > 1e-4)[0][0] for r in (a, b))
ps = float((W / "player_start.log").read_text())
sw = {l.split()[0]: float(l.split()[1]) - ps for l in (W / "switch.log").read_text().splitlines()}
print("output moved to B at %.1fs, back to A at %.1fs" % (sw["switch"], sw["switch_back"]))
print("profile A sounds like L@1k 0.399 R@300 0.107; profile B like 0.038 / 0.075")
print(" t(s) |  device A  L@1k  R@300 |  device B  L@1k  R@300")
for t in np.arange(0.5, 7.6, 0.5):
    i = t0 + int(t * fs); w = int(0.3 * fs)
    seg = lambda r: (amp(r[i:i + w, 0], 1000, fs), amp(r[i:i + w, 1], 300, fs))
    print("%5.1f |      %.4f %.4f |      %.4f %.4f" % ((t,) + seg(a) + seg(b)))

# Ringing during a switch would show up as peaks far above what either profile produces.
print()
print("largest sample per channel over the whole recording (profile A: L ~0.4 R ~0.11, B: L ~0.05 R ~0.08):")
for name, r in (("device A", a), ("device B", b)):
    print("  %s  L %.3f  R %.3f" % (name, np.abs(r[:, 0]).max(), np.abs(r[:, 1]).max()))
