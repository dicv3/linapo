#!/bin/bash
# Per-device profile switching on a real (private, headless) PipeWire.
# Run through:  dbus-run-session -- ./run.sh      (after ./prepare.sh)
# Needs: pipewire, wireplumber, pw-cat, wpctl, python3 + numpy. Results: see analyze.py.
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$HERE/work"
cd "$ROOT"
export XDG_RUNTIME_DIR="$ROOT/run" XDG_CONFIG_HOME="$ROOT/cfg"
L="${LINAPO:-$HERE/../../../target/debug/linapo}"
rm -f $XDG_RUNTIME_DIR/pipewire-0*
PIPEWIRE_CONFIG_DIR="$ROOT/pw" pipewire > pw.log 2>&1 &
for i in $(seq 1 50); do [ -S $XDG_RUNTIME_DIR/pipewire-0 ] && break; sleep 0.1; done
wireplumber > wp.log 2>&1 &
sleep 3
idof() { pw-dump | python3 -c 'import json,sys
for o in json.load(sys.stdin):
    if o.get("info",{}).get("props",{}).get("node.name")=="'$1'": print(o["id"])'; }
A=$(idof dev_a); B=$(idof dev_b)
wpctl set-default $A; sleep 1
echo "default sink -> dev_a (id $A); output device seen by linapo: $($L device 2>&1)"
timeout -s INT 16 pw-cat -r recA.wav -P '{ node.name=recA node.autoconnect=false }' --channels 2 --rate 48000 --format f32 > /dev/null 2>&1 &
RA=$!
timeout -s INT 16 pw-cat -r recB.wav -P '{ node.name=recB node.autoconnect=false }' --channels 2 --rate 48000 --format f32 > /dev/null 2>&1 &
RB=$!
sleep 1
pw-link dev_a:monitor_FL recA:input_FL; pw-link dev_a:monitor_FR recA:input_FR
pw-link dev_b:monitor_FL recB:input_FL; pw-link dev_b:monitor_FR recB:input_FR
$L watch --interval 0.4 > watch.log 2>&1 &
W=$!
sleep 2
date +%s.%N > player_start.log
timeout 20 pw-cat -p tone9.wav -P '{ node.name=player }' --target effect_input.linapo --channels 2 --rate 48000 > /dev/null 2>&1 &
P=$!
sleep 3.5
echo "switch $(date +%s.%N)" > switch.log
echo "--- links before switch:"; pw-link -l | grep -A2 "effect_output.linapo:output_FL" | head -3
wpctl set-default $B
sleep 3.5
echo "--- links after switch:"; pw-link -l | grep -A2 "effect_output.linapo:output_FL" | head -3
echo "switch_back $(date +%s.%N)" >> switch.log
wpctl set-default $A
tail --pid=$P -f /dev/null
sleep 0.5; kill -INT $RA $RB; sleep 1; kill $W
echo "--- watcher output"; cat watch.log
echo "--- active profile at end: $($L profile active)"
pkill wireplumber; pkill pipewire
