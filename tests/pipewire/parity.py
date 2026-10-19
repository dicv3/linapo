#!/usr/bin/env python3
"""PipeWire parity test: does a real PipeWire sound like the offline engine?

Usage:  tests/pipewire/parity.py SETUP.json [--linapo PATH] [--seconds 4] [--tolerance 0.05]

It starts a PRIVATE headless PipeWire + WirePlumber (your normal audio is not touched),
installs SETUP.json as a virtual sink in front of a null output, plays a multitone
(each input channel carries its own tones) through it, records the output, renders the
same input offline with `linapo process`, and compares every output channel at every
tone frequency. Exit status 0 means the worst difference is within --tolerance dB.

Needs: pipewire, wireplumber, pw-cat, pw-cli, dbus-run-session, python3 + numpy.
"""
import argparse, json, os, shutil, signal, struct, subprocess, sys, tempfile, time, wave
from pathlib import Path

POS = {"L": "FL", "R": "FR", "C": "FC", "SUB": "LFE", "RL": "RL", "RR": "RR", "SL": "SL", "SR": "SR"}
TONES = [100, 250, 500, 1000, 2000, 4000, 8000]
FS = 48000


def position(ch):
    return POS.get(ch) or f"AUX{max(int(ch[2:]) - 1, 0)}"


def find_linapo(arg):
    if arg:
        return arg
    root = Path(__file__).resolve().parents[2]
    for p in (root / "target/release/linapo", root / "target/debug/linapo"):
        if p.exists():
            return str(p)
    return shutil.which("linapo") or sys.exit("linapo binary not found; build it or pass --linapo")


def tones_for(i):
    return sorted({TONES[(i + 3 * j) % len(TONES)] for j in range(3)})


def write_input(path, n_in, seconds):
    import numpy as np
    t = np.arange(FS * seconds) / FS
    cols = []
    for i in range(n_in):
        cols.append(sum(0.05 * np.sin(2 * np.pi * f * t) for f in tones_for(i)))
    data = (np.stack(cols, axis=1) * 32767).astype("<i2")
    with wave.open(str(path), "wb") as w:
        w.setnchannels(n_in); w.setsampwidth(2); w.setframerate(FS)
        w.writeframes(data.tobytes())


def read_float_wav(path):
    import numpy as np
    d = Path(path).read_bytes(); i = 12; fmt = None
    while i < len(d):
        cid, sz = d[i:i + 4], struct.unpack("<I", d[i + 4:i + 8])[0]
        if cid == b"fmt ": fmt = struct.unpack("<HHIIHH", d[i + 8:i + 24])
        if cid == b"data": return np.frombuffer(d[i + 8:i + 8 + sz], dtype="<f4").reshape(-1, fmt[1]), fmt[2]
        i += 8 + sz + (sz & 1)
    raise SystemExit(f"no data chunk in {path}")


def amplitude(x, f, fs):
    import numpy as np
    t = np.arange(len(x)) / fs
    return 2 * abs(np.sum(x * np.exp(-2j * np.pi * f * t))) / len(x)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("setup"); ap.add_argument("--linapo"); ap.add_argument("--seconds", type=int, default=4)
    ap.add_argument("--tolerance", type=float, default=0.05)
    args = ap.parse_args()

    if "DBUS_SESSION_BUS_ADDRESS" not in os.environ and shutil.which("dbus-run-session"):
        os.execvp("dbus-run-session", ["dbus-run-session", "--", sys.executable, *sys.argv])

    import numpy as np
    linapo = find_linapo(args.linapo)
    setup = json.load(open(args.setup))
    ins, outs = setup["input_channels"], setup["output_channels"]
    tmp = Path(tempfile.mkdtemp(prefix="linapo-parity-"))
    run, pwconf, cfg = tmp / "run", tmp / "pw", tmp / "cfg"
    for d in (run, pwconf / "pipewire.conf.d"):
        d.mkdir(parents=True)
    run.chmod(0o700)
    default = next((p for p in ("/usr/share/pipewire/pipewire.conf", "/etc/pipewire/pipewire.conf") if os.path.exists(p)), None)
    if not default: sys.exit("default pipewire.conf not found")
    shutil.copy(default, pwconf / "pipewire.conf")
    (pwconf / "pipewire.conf.d/10-null.conf").write_text(
        "context.objects = [ { factory = adapter args = { factory.name = support.null-audio-sink node.name = test_out "
        "media.class = Audio/Sink object.linger = true audio.position = [ %s ] "
        "monitor.channel-volumes = true monitor.passthrough = true } } ]\n" % " ".join(position(c) for c in outs))
    env = {**os.environ, "XDG_CONFIG_HOME": str(cfg)}
    subprocess.run([linapo, "install", args.setup, "--target", "test_out", "--no-restart"], env=env, check=True, stdout=subprocess.DEVNULL)
    shutil.copy(cfg / "pipewire/pipewire.conf.d/99-linapo.conf", pwconf / "pipewire.conf.d/20-linapo.conf")

    inp, rec, off = tmp / "in.wav", tmp / "rec.wav", tmp / "off.wav"
    write_input(inp, len(ins), args.seconds)
    subprocess.run([linapo, "process", args.setup, str(inp), str(off)], check=True, stderr=subprocess.DEVNULL)

    penv = {**os.environ, "XDG_RUNTIME_DIR": str(run)}
    procs = []
    try:
        procs.append(subprocess.Popen(["pipewire"], env={**penv, "PIPEWIRE_CONFIG_DIR": str(pwconf)}, stdout=subprocess.DEVNULL, stderr=open(tmp / "pipewire.log", "w")))
        for _ in range(100):
            if (run / "pipewire-0").exists(): break
            time.sleep(0.1)
        procs.append(subprocess.Popen(["wireplumber"], env=penv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        for _ in range(150):
            nodes = subprocess.run(["pw-cli", "ls", "Node"], env=penv, capture_output=True, text=True).stdout
            if "effect_input.linapo" in nodes and "test_out" in nodes: break
            time.sleep(0.1)
        else:
            sys.exit("virtual sink did not appear; see %s" % (tmp / "pipewire.log"))
        time.sleep(2)
        recorder = subprocess.Popen(["pw-cat", "-r", str(rec), "-P", "{ node.name=rec stream.capture.sink=true }", "--target", "test_out",
                                     "--channels", str(len(outs)), "--rate", str(FS), "--format", "f32"], env=penv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        time.sleep(1.5)
        player = subprocess.Popen(["pw-cat", "-p", str(inp), "-P", "{ node.name=player }", "--target", "effect_input.linapo"], env=penv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        player.wait(timeout=args.seconds + 20)
        time.sleep(0.5)
        recorder.send_signal(signal.SIGINT); recorder.wait(timeout=10)
    finally:
        for p in reversed(procs):
            p.terminate()
        for p in procs:
            try: p.wait(timeout=5)
            except subprocess.TimeoutExpired: p.kill()

    a, fs = read_float_wav(rec); b, _ = read_float_wav(off)
    live = np.where(np.abs(a).max(axis=1) > 1e-3)[0]
    if len(live) == 0: sys.exit("recording is silent: the virtual sink passed no audio")
    t0 = live[0]
    lo, hi = int(0.5 * fs), int((args.seconds - 1.5) * fs)
    ra, rb = a[t0 + lo:t0 + hi], b[lo:hi]
    freqs = sorted({f for i in range(len(ins)) for f in tones_for(i)})
    print("PipeWire vs engine, difference in dB (blank = silent on both)\n%-6s" % "out" + "".join("%8d" % f for f in freqs))
    worst = 0.0
    for c, name in enumerate(outs):
        row = ""
        for f in freqs:
            x, y = amplitude(ra[:, c], f, fs), amplitude(rb[:, c], f, fs)
            if max(x, y) < 1e-3 or min(x, y) < 1e-4:   # silent, or a null/notch where ratios are meaningless
                row += "%8s" % "."
                continue
            d = 20 * np.log10(x / y); worst = max(worst, abs(d)); row += "%+8.3f" % d
        print("%-6s%s" % (name, row))
    ok = worst <= args.tolerance
    print("\nworst difference %.3f dB (tolerance %.3f dB): %s" % (worst, args.tolerance, "PASS" if ok else "FAIL"))
    shutil.rmtree(tmp, ignore_errors=True)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
