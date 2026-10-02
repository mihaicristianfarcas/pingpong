# Ping + Pong against Moonlight + Apollo

pingpong set out to replace a Moonlight + Apollo setup feature for feature.
This is the checklist, compared against Moonlight on a 14" MacBook Pro
(3024x1890, 120 fps, 100 Mbit/s, V-Sync and frame pacing on, 7.1 audio,
connection warnings, gamepad mouse, keep awake, mute on focus loss) and
Apollo on a Windows 11 PC with an RTX 3070 Ti (defaults, apps Desktop and
Steam Big Picture). Numbers are in [benchmarks.md](benchmarks.md).

Status: **done** (built and verified), **no** (not built, with the
reason), **n/a** (does not apply).

## Connecting

| Moonlight + Apollo | Ping + Pong | Status |
|---|---|---|
| Hosts found on the LAN (mDNS) | Same; every address a host announces is kept, LAN first, VPN overlays (such as Tailscale) last | done |
| PIN pairing | PIN shown in Ping, entered in Pong's window or web UI; hybrid post-quantum (SPAKE2 + ML-KEM-768) | done |
| Add a host by address | Same | done |
| Encryption (AES-GCM, pairing certificates) | WireGuard tunnel with ML-KEM (post-quantum) | done |
| Reaching the host from outside (port forwarding, a VPN) | Built in: sealed rendezvous records and hole punching, no port forwarding ([networking.md](networking.md)); from a phone hotspot, no loss, 40 ms round trip. UPnP / NAT-PMP port mapping too, as Apollo | done |
| Wake-on-LAN | Pong announces its adapters; Ping's Wake (or a click on an offline host), `ping wake NAME`. The test host answered 6 s after the packets and streamed its lock screen 1 s later (see [usage.md](usage.md#wake-on-lan) for what the host's BIOS and adapter need) | done |

## Video

| Moonlight + Apollo | Ping + Pong | Status |
|---|---|---|
| 3024x1890 native, 120 fps, 100 Mbit/s | Same; 115.6 fps shown vs Moonlight's 110.0 under full load. Host on Ethernet: 0% loss at 113 Mbit/s, 117 of 118 fps shown, decode to glass 10 ms | done |
| HEVC, H.264 | Same | done |
| AV1 | The RTX 3070 Ti cannot encode it | n/a |
| HDR, YUV 4:4:4 | Not built | no |
| V-Sync, frame pacing | Moonlight's pacer: display-link tick, oldest queued frame | done |
| Loss recovery (reference invalidation) | Same, plus adaptive FEC that learns the link's own loss | done |
| Automatic bitrate | Adapts to congestion, not to random loss | done |
| Statistics overlay (Ctrl-Alt-Shift-S) | Same, plus host capture to glass | done |
| Connection warnings | Same thresholds in spirit; no false warning at start | done (seen rendering) |
| Full screen / window (Ctrl-Alt-Shift-X) | Same | done |
| Session start | 0.9-1.5 s with the host's monitor awake or unplugged; ~5 s when it is asleep or off, as with Apollo (Windows spends 3.9 s on the sleeping monitor) | done |

## Audio

| Moonlight + Apollo | Ping + Pong | Status |
|---|---|---|
| Stereo, 5.1, 7.1 | Same, Opus multistream in Moonlight's channel order; a Mac host from a Core Audio tap of a surround output, as Sunshine's from BlackHole | done (Mac host: through BlackHole 16ch) |
| Sound only on the client (host muted) | Virtual sink for the session, default device restored after | done |
| Mute when Moonlight is in the background | Same | done |

## Input

| Moonlight + Apollo | Ping + Pong | Status |
|---|---|---|
| Keyboard and mouse capture, Ctrl-Alt-Shift-Z | Same | done |
| The host's pointer drawn into the picture (Apollo, from Desktop Duplication) | Same, on every host (Desktop Duplication's pointer drawn in on the GPU, ScreenCaptureKit's, XFixes') | done |
| Relative mouse by default; "Optimize mouse for remote desktop" sends positions | Relative by default; positions with the mode toggle | done |
| Mouse mode toggle (Ctrl-Alt-Shift-M) | Same | done |
| Command as the Windows key (option) | Same; always Command on a Mac host | done |
| Paste the clipboard (Ctrl-Alt-Shift-V) | Same, Unicode, survives 10% burst loss | done |
| (Moonlight has none) Clipboard shared both ways | Text, images, files and folders; password managers' copies stay put ([usage.md](usage.md#sharing-the-clipboard)) | done, beyond parity |
| Controllers, several, with rumble | Same (virtual Xbox 360 pads); rumble reaches the pad ~7 ms after the game sets it | done (Xbox Wireless Controller) |
| Controller's Xbox button | Reaches the host as Guide (read from the pad's HID reports, as Moonlight does); macOS 26 opens its Games overlay too, as with Moonlight | done |
| Controller as a mouse (hold Start) | Same | done |
| Swap mouse buttons, reverse scrolling | Not built | no |

## Host

| Apollo | Pong | Status |
|---|---|---|
| Virtual display at the client's mode | SudoVDA, primary, the host's monitors off for the session (Apollo's default) | done |
| Apps: Desktop, Steam Big Picture (closed at the end) | Same | done |
| Runs at boot, streams UAC prompts and the lock screen | Windows service; sessions also start while the host is locked (after sleep) | done |
| Web UI | Pairing, clients, settings, logs, ending a session | done |
| Host shutting down or restarting mid-stream | Ping says so at once | done |
| Host software restarting mid-stream | Ping resumes by itself (~10 s) | done |
| Streams with its monitor asleep, off or unplugged | Yes; sessions end in under 0.1 s with no monitor (6 of 6) | done |
| Network priority on Wi-Fi (QoS) | Not possible on Pong's dual-stack socket | no |
| (Apollo has no macOS host) | Pong on macOS: virtual display at the client's size, HEVC, sound, keyboard and mouse | done: see [platforms/macos.md](platforms/macos.md) |

## Beyond Moonlight + Apollo

- Clients and hosts on macOS, Windows and Linux, from one code base.
- Everything in one post-quantum tunnel (WireGuard with ML-KEM), paired
  with a hybrid post-quantum PIN exchange.
- Reaching the host from anywhere without port forwarding or a VPN.
- The clipboard shared both ways, files included.
- AI agents as clients of their own, held to the host's rules
  ([ai-agents.md](ai-agents.md)).

## Not built

HDR, YUV 4:4:4, swapped mouse buttons and reversed scrolling. Phones,
tablets and TVs are out of scope: pingpong is for desktops.
