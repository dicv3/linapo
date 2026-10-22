#!/bin/bash
# Creates ./work with two null output devices, profiles A (+6 dB at 1 kHz) and B (low-pass + notch),
# bindings dev_a -> A and dev_b -> B, and the engine config installed (not activated).
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$HERE/work"
L="${LINAPO:-$HERE/../../../target/debug/linapo}"
rm -rf "$ROOT"; mkdir -p "$ROOT/pw/pipewire.conf.d" "$ROOT/run" "$ROOT/cfg"; chmod 700 "$ROOT/run"
cd "$ROOT"
cp /usr/share/pipewire/pipewire.conf pw/pipewire.conf
cat > pw/pipewire.conf.d/10-null.conf <<'EOF'
context.objects = [
  { factory = adapter args = { factory.name = support.null-audio-sink node.name = dev_a node.description = "Device A" media.class = Audio/Sink object.linger = true audio.position = [ FL FR ] monitor.channel-volumes = true monitor.passthrough = true priority.session = 2000 } }
  { factory = adapter args = { factory.name = support.null-audio-sink node.name = dev_b node.description = "Device B" media.class = Audio/Sink object.linger = true audio.position = [ FL FR ] monitor.channel-volumes = true monitor.passthrough = true priority.session = 1000 } }
]
EOF
cat > A.json <<'EOF'
{ "input_channels":["L","R"], "output_channels":["L","R"], "matrix":[[1,0],[0,1]], "limiter":false,
  "eqs":[{"name":"boost","channels":["L","R"],"filters":[{"kind":"peaking","fc_hz":1000,"gain_db":6,"q":1.0}]}] }
EOF
cat > B.json <<'EOF'
{ "input_channels":["L","R"], "output_channels":["L","R"], "matrix":[[1,0],[0,1]], "limiter":false, "master_gain_db": -2,
  "eqs":[{"name":"lowpass","channels":["L","R"],"filters":[{"kind":"low_pass","fc_hz":500,"q":0.7071},{"kind":"notch","fc_hz":3000,"q":4}]}] }
EOF
python3 - <<'PY'
import wave, struct, math
fs=48000
w=wave.open('tone9.wav','wb'); w.setnchannels(2); w.setsampwidth(2); w.setframerate(fs)
w.writeframes(b''.join(struct.pack('<hh', int(0.2*32767*math.sin(2*math.pi*1000*i/fs)), int(0.1*32767*math.sin(2*math.pi*300*i/fs))) for i in range(fs*9)))
w.close()
PY
export XDG_CONFIG_HOME="$ROOT/cfg"
"$L" profile save A A.json; "$L" profile save B B.json
"$L" bindings set dev_a A; "$L" bindings set dev_b B; "$L" bindings auto on; "$L" profile use A >/dev/null
"$L" install "$("$L" profile path A)" --no-restart
cp cfg/pipewire/pipewire.conf.d/99-linapo.conf pw/pipewire.conf.d/20-linapo.conf
for X in A B; do "$L" process $X.json tone9.wav off_$X.wav 2>/dev/null; done
echo "prepared in $ROOT; now: dbus-run-session -- $HERE/run.sh && python3 $HERE/analyze.py"
