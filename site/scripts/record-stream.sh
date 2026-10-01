#!/usr/bin/env bash
# The page's stream video: a real Pong-to-Ping stream recorded in the Linux
# container, both screens side by side, cut into a seamless loop for
# public/media, and Ping's per-second statistics averaged for
# src/lib/facts.ts (RECORDING).
#
#   site/scripts/record-stream.sh
#
# Needs tools/linux-dev working (Docker; on a Mac, colima) and ffmpeg here.
set -euo pipefail
SITE=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
REPO=$(cd "$SITE/.." && pwd)
RUN=$REPO/target/linux-out/site-stream
MEDIA=$SITE/public/media

"$REPO/tools/linux-dev" cargo build --locked --release -p pong -p ping-core --bin ping
"$REPO/tools/linux-dev" bash /src/site/scripts/stream-in-container.sh

# A loop that does not jump: 23 s of the recording, from 7 s. The scene
# repeats every 23 s (clock-scene.py), so the clip's last frame leads into
# its first as any frame leads into the next. Each screen is scaled to
# 960x540, with 48 px between them that the page covers with the gap
# between its two screens (src/components/Latency.astro).
mkdir -p "$MEDIA"
ffmpeg -loglevel error -y -i "$RUN/both.mp4" -filter_complex "
    [0:v]split[a][b];
    [a]crop=1280:720:0:0,scale=960:540:flags=lanczos,pad=1008:540[l];
    [b]crop=1280:720:1280:0,scale=960:540:flags=lanczos[r];
    [l][r]hstack,trim=7:30,setpts=PTS-STARTPTS,format=yuv420p[v]" \
    -map "[v]" -an -c:v libx264 -preset slow -crf 22 -profile:v high -movflags +faststart \
    "$MEDIA/stream-split.mp4"
ffmpeg -loglevel error -y -ss 11.2 -i "$MEDIA/stream-split.mp4" -frames:v 1 -q:v 3 "$MEDIA/stream-split.jpg"

# Ping's per-second statistics over the recording (its first two seconds,
# the stream settling, left out). Both sides run in one VM, so when the VM
# is starved (the Mac busy elsewhere) host and client stall together and
# the clip stutters: that is the VM, not the stream, and it is recorded
# again rather than averaged in.
stats=$(grep "second received_fps" "$RUN/ping.log" | sed -n '3,34p')
echo "Averages over the recording, for src/lib/facts.ts:"
echo "$stats" | awk '{
    for (i = 1; i <= NF; i++) { split($i, kv, "=");
        if (kv[1] ~ /^(presented_fps|lost_frames|host_ms|network_ms|decode_ms|render_ms|end_to_end_ms|mbps)$/) { s[kv[1]] += kv[2]; n[kv[1]]++ } } }
    END { for (k in s) printf "  %s = %.2f\n", k, s[k] / n[k] }'
worst=$(echo "$stats" | sed -E 's/.*presented_fps=([0-9]+).*/\1/' | sort -n | head -1)
echo "  slowest second: $worst fps"
if (( worst < 55 )); then
    echo "A second fell below 55 fps: the VM stalled. Record again." >&2
    exit 1
fi
