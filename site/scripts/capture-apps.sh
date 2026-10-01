#!/usr/bin/env bash
# The page's screenshots of Ping and Pong, from the apps' demo mode
# (PING_UI_DEMO / PONG_UI_DEMO, docs/ui.md) in their dark appearance, as
# WebP in src/assets/shots. macOS; each window is captured as it is
# (screencapture), so the terminal needs Screen Recording. The hero's two
# windows (src/assets/hero/) are screenshots taken by hand.
#
#   cargo build --release -p ping-app -p pong-app
#   site/scripts/capture-apps.sh
#
# The data folders are empty and temporary: nothing of this computer's
# hosts or settings reaches a picture. Ping's sample session is shown the
# made-up desktop in agent-screen.webp (drawn from agent-screen.html, its
# clicks where the sample's land).
set -euo pipefail
SITE=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
REPO=$(cd "$SITE/.." && pwd)
SHOTS=$SITE/src/assets/shots
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$SHOTS"
node -e 'require(process.argv[1])(process.argv[2]).png().toFile(process.argv[3])' \
    "$SITE/node_modules/sharp" "$SITE/scripts/agent-screen.webp" "$TMP/screen.png"

shot() { # app name steps
    local data out=$TMP/$2.png
    data=$(mktemp -d "$TMP/data.XXXX")
    if [[ $1 == ping ]]; then
        PING_DATA_DIR=$data PINGPONG_APPEARANCE=dark PING_UI_DEMO_SCREEN=$TMP/screen.png \
            PING_UI_DEMO="$3,snapshot=$out,quit" "$REPO/target/release/ping-app" >/dev/null 2>&1
    else
        PONG_DATA_DIR=$data PINGPONG_APPEARANCE=dark \
            PONG_UI_DEMO="$3,snapshot=$out,quit" "$REPO/target/release/pong-app" >/dev/null 2>&1
    fi
    node -e 'require(process.argv[1])(process.argv[2]).webp({ quality: 92, alphaQuality: 100, effort: 6 }).toFile(process.argv[3])' \
        "$SITE/node_modules/sharp" "$out" "$SHOTS/$2.webp"
    echo "$2"
}

shot ping ping-ask "agents=sample-ask"
shot ping ping-video "settings=video"
shot ping ping-input "settings=input"
for page in devices network agents; do
    shot pong "pong-$page" "sample,$page"
done
