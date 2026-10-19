#!/bin/bash
# runs inside dbus-run-session.  usage: inner.sh <mid-script> <tone.wav> <rec.wav>
cd "$(dirname "$0")"; mkdir -p work; cd work
export XDG_RUNTIME_DIR=/tmp/e2e/run
rm -f $XDG_RUNTIME_DIR/pipewire-0*
PIPEWIRE_CONFIG_DIR=/tmp/e2e/pw pipewire > pw.log 2>&1 &
for i in $(seq 1 50); do [ -S $XDG_RUNTIME_DIR/pipewire-0 ] && break; sleep 0.1; done
wireplumber > wp.log 2>&1 &
sleep 3
Xvfb :99 -screen 0 1280x800x24 > xvfb.log 2>&1 &
sleep 1.5
DISPLAY=:99 LIBGL_ALWAYS_SOFTWARE=1 HOME=/tmp/e2e/home XDG_CONFIG_HOME=/tmp/e2e/xdgcfg /home/claude/linapo/target/debug/linapo-gui /tmp/e2e/setup.json > gui.log 2>&1 &
sleep 7
DISPLAY=:99 import -window root shot-start.png
if [ -n "$2" ]; then
  timeout -s INT 18 pw-cat -r "$3" -P '{ node.name=rec stream.capture.sink=true }' --target test_out --channels 2 --rate 48000 --format f32 > rec.log 2>&1 &
  REC=$!
  sleep 1.5
  timeout 20 pw-cat -p "$2" -P '{ node.name=player }' --target effect_input.linapo --channels 2 --rate 48000 > play.log 2>&1 &
  PLAY=$!
  sleep 2.5
  [ -n "$1" ] && DISPLAY=:99 bash "$1"
  tail --pid=$PLAY -f /dev/null
  sleep 0.5; kill -INT $REC; sleep 1
fi
DISPLAY=:99 import -window root shot-end.png
pkill linapo-gui; pkill Xvfb; pkill wireplumber; pkill pipewire
