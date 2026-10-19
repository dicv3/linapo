export DISPLAY=:99
echo "t0 $(date +%s.%N)" > mid.log
xdotool mousemove 162 223 click 1; echo "clicked L->R $(date +%s.%N)" >> mid.log
sleep 1.8
xdotool mousemove 88 223 click 1;  echo "clicked L->L $(date +%s.%N)" >> mid.log
sleep 1.8
xdotool mousemove 1026 21 mousedown 1; sleep 0.2; xdotool mousemove 1010 21; sleep 0.1; xdotool mousemove 987 21; sleep 0.2; xdotool mouseup 1; echo "dragged master $(date +%s.%N)" >> mid.log
sleep 0.5
DISPLAY=:99 import -window root shot-mid.png
