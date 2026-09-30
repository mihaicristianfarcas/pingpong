#!/bin/bash
# A person streams linux-desk from screen :2 for SECS seconds.
pgrep -f "^Xvfb :2" >/dev/null || (Xvfb :2 -screen 0 1280x800x24 -nolisten tcp >/dev/null 2>&1 &)
sleep 1
export DISPLAY=:2 PING_DATA_DIR=/src/target/linux-out/agent/ping RUST_LOG=info,mainline=error
export PING_TEST_INPUT="wait ${1:-6}000; quit"
nohup /target/debug/ping stream linux-desk --windowed --size 1280x800 --no-audio > /src/target/linux-out/agent/person.log 2>&1 &
