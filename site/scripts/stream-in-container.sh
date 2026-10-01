#!/usr/bin/env bash
# Inside the pingpong-linux container (run by record-stream.sh): a running
# clock on X screen :1 (clock-scene.py), Pong streaming it, Ping showing it
# on :2, and both screens recorded side by side (grab-screens.py). Ping
# logs its per-second statistics without drawing them over the picture.
# After tools/linux/loopback-test, with release builds.
set -euo pipefail
SECS=${SECS:-36}
bin=/target/release
HERE=/src/site/scripts
OUT=/src/target/linux-out/site-stream
rm -rf "$OUT"
mkdir -p "$OUT/pong" "$OUT/ping"
export DEBIAN_FRONTEND=noninteractive
# The recorder and the scene need packages the image does not have; the
# container is thrown away afterwards.
apt-get update -qq >/dev/null &&
    apt-get install -y -qq --no-install-recommends ffmpeg python3-pygame >/dev/null

export PONG_DATA_DIR=$OUT/pong PING_DATA_DIR=$OUT/ping NO_COLOR=1
cat >"$PONG_DATA_DIR/config.toml" <<'TOML'
name = "gaming-pc"
port = 47810
pairing_port = 47811
web_port = 47812
internet_access = false
TOML

# Each screen's framebuffer in a file, for grab-screens.py.
mkdir -p /tmp/fb1 /tmp/fb2
Xvfb :1 -screen 0 1280x720x24 -fbdir /tmp/fb1 -nolisten tcp >/dev/null 2>&1 &
Xvfb :2 -screen 0 1280x720x24 -fbdir /tmp/fb2 -nolisten tcp >/dev/null 2>&1 &
sleep 1
DISPLAY=:1 SDL_AUDIODRIVER=dummy python3 "$HERE/clock-scene.py" $((SECS + 20)) >"$OUT/scene.log" 2>&1 &

host_keys=$("$bin/pong" identity | sed -E 's/.*= "(.*)"/\1/' | paste -sd' ')
client_keys=$("$bin/ping" identity)
# shellcheck disable=SC2086
"$bin/pong" add-client macbook $client_keys >/dev/null
# shellcheck disable=SC2086
"$bin/ping" add-host gaming-pc 127.0.0.1:47810 $host_keys >/dev/null

DISPLAY=:1 RUST_LOG=info "$bin/pong" host >"$OUT/pong.log" 2>&1 &
pong=$!
sleep 3

export PING_TEST_INPUT="wait $(((SECS + 6) * 1000)); quit"
DISPLAY=:2 RUST_LOG=info,mainline=error,ping_core::stats=debug \
    "$bin/ping" stream gaming-pc --windowed --size 1280x720 --fps 60 >"$OUT/ping.log" 2>&1 &
ping=$!
sleep 4

# Both screens at the same instants: the host's on the left, the client's
# on the right (grab-screens.py stacks them; ffmpeg sets them side by side).
python3 "$HERE/grab-screens.py" /tmp/fb1/Xvfb_screen0 /tmp/fb2/Xvfb_screen0 "$SECS" 2>"$OUT/grab.log" |
    ffmpeg -loglevel error -y -f rawvideo -pix_fmt bgr0 -s 1280x1440 -framerate 60 -i - \
        -filter_complex "[0]split[t][b];[t]crop=1280:720:0:0[l];[b]crop=1280:720:0:720[r];[l][r]hstack" \
        -c:v libx264 -preset veryfast -crf 12 -pix_fmt yuv420p "$OUT/both.mp4"
cat "$OUT/grab.log"

wait "$ping" || echo "ping exited with $?"
kill -INT "$pong" 2>/dev/null || true
for _ in $(seq 100); do kill -0 "$pong" 2>/dev/null || break; sleep 0.2; done
kill -9 "$pong" 2>/dev/null || true
