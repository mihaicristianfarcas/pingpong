# Ping + Pong against Moonlight + Sunshine

pingpong set out to replace a Moonlight + Apollo setup feature for feature,
and now measures itself against Moonlight + Sunshine: Apollo is a fork of
Sunshine that adds virtual displays and has not moved in a long while,
while Sunshine keeps improving. This is the checklist, compared against
Moonlight on a 14" MacBook Pro (3024x1890, 120 fps, 100 Mbit/s, V-Sync and
frame pacing on, 7.1 audio, connection warnings, gamepad mouse, keep awake,
mute on focus loss) and, on a Windows 11 PC with an RTX 3070 Ti, Apollo
(defaults, apps Desktop and Steam Big Picture), whose video path is
Sunshine's. Sunshine's own source was read for what it does differently or
added since. Numbers are in [benchmarks.md](benchmarks.md).

Status: **done** (built and verified), **built** (built, not yet run where
it applies), **no** (not built, with the reason), **n/a** (does not apply).

## Connecting

| Moonlight + Sunshine | Ping + Pong | Status |
|---|---|---|
| Hosts found on the LAN (mDNS) | Same; every address a host announces is kept, LAN first, VPN overlays (such as Tailscale) last | done |
| PIN pairing | PIN shown in Ping, entered in Pong's window or web UI; hybrid post-quantum (SPAKE2 + ML-KEM-768) | done |
| Add a host by address | Same | done |
| Encryption (AES-GCM, pairing certificates) | WireGuard tunnel with ML-KEM (post-quantum) | done |
| Reaching the host from outside (UPnP when on, port forwarding, a VPN) | Built in: sealed rendezvous records and hole punching, no port forwarding ([networking.md](networking.md)); from a phone hotspot, no loss, 40 ms round trip. UPnP / NAT-PMP port mapping too, on by default (Sunshine's is off) | done |
| Wake-on-LAN | Pong announces its adapters; Ping's Wake (or a click on an offline host), `ping wake NAME`. The test host answered 6 s after the packets and streamed its lock screen 1 s later (see [usage.md](usage.md#wake-on-lan) for what the host's BIOS and adapter need) | done |

## Video

| Moonlight + Sunshine | Ping + Pong | Status |
|---|---|---|
| 3024x1890 native, 120 fps, 100 Mbit/s | Same; 115.6 fps shown vs Moonlight's 110.0 under full load (against Apollo, Sunshine's video path). Host on Ethernet: 0% loss at 113 Mbit/s, 117 of 118 fps shown, decode to glass 10 ms | done |
| The client display's exact refresh (`clientRefreshRateX100`, 59.94 Hz), the host capturing at it | Same, in millihertz from negotiation to the virtual display, the capture cadence and the encoder | done (no 59.94 Hz display here) |
| HEVC, H.264 | Same | done |
| AV1 | Pong encodes it with NVENC on GPUs that have it (not the RTX 3070 Ti); Ping decodes it in hardware on a Mac with an AV1 decoder (M3 and later; its Video codec setting offers AV1 there), and with FFmpeg on Windows and Linux (`--codec av1`, untested) | done on a Mac (decoder tested on an M4 Pro); n/a on an RTX 3070 Ti host |
| HDR (HEVC Main10, BT.2020, PQ) | Same: Ping on a Mac with an HDR display draws it in a BT.2100 PQ layer with the host's metadata; a Windows host switches its virtual display to HDR and converts the FP16 desktop to PQ; a Mac host (macOS 15+) makes an HDR virtual display and captures it in HDR10, which Sunshine's macOS host does not | done Mac to Mac; built on Windows |
| YUV 4:4:4 | Same: NVENC (Windows), NVENC or x264 (Linux); Ping decodes it on Apple silicon and Linux. Not with HDR on Windows (NVENC takes 10-bit 4:4:4 only from CUDA; Sunshine uses CUDA there) | done Linux to Linux, Mac decode tested; built on Windows |
| NVENC: P1, ultra-low latency, single-frame VBV, quarter-resolution two pass, reference invalidation | Same | done |
| NVENC split-frame encoding on GPUs with two encoders | The driver decides, Sunshine's default | done (one encoder here) |
| The NVIDIA driver set for streaming (full power for the host, OpenGL/Vulkan through DXGI) | Same, put back when Pong stops | built |
| V-Sync, frame pacing | Moonlight's pacer: display-link tick, oldest queued frame | done |
| Loss recovery (reference invalidation) | Same, plus adaptive FEC that learns the link's own loss (Sunshine sends a fixed 20%) | done |
| Automatic bitrate | Adapts to congestion, not to random loss | done |
| Statistics overlay (Ctrl-Alt-Shift-S) | Same, plus host capture to glass | done |
| Connection warnings | Same thresholds in spirit; no false warning at start | done (seen rendering) |
| Full screen / window (Ctrl-Alt-Shift-X) | Same | done |
| Session start | 0.9-1.5 s with the host's monitor awake or unplugged; ~5 s when it is asleep or off, as with Apollo (Windows spends 3.9 s on the sleeping monitor) | done |
| A still desktop re-encoded so it sharpens | At a fifth of the frame rate (Sunshine: half): Ping needs no steady stream of frames | done |

## Audio

| Moonlight + Sunshine | Ping + Pong | Status |
|---|---|---|
| Stereo, 5.1, 7.1 | Same, Opus multistream in Moonlight's channel order; a Mac host from a Core Audio tap of a surround output, as Sunshine's from BlackHole | done (Mac host: through BlackHole 16ch) |
| Sound only on the client (host muted) | Virtual sink for the session, default device restored after | done |
| Mute when Moonlight is in the background | Same | done |

## Input

| Moonlight + Sunshine | Ping + Pong | Status |
|---|---|---|
| Keyboard and mouse capture, Ctrl-Alt-Shift-Z | Same | done |
| A held key repeats (Sunshine repeats on the host, 500 ms then 24.9 a second) | Same, at the delay and rate of the keyboard in front of you (sent with the session) | done in tests; not run on a host with a key held |
| The host's pointer drawn into the picture | Same, on every host (Desktop Duplication's pointer drawn in on the GPU, ScreenCaptureKit's, XFixes'); on an HDR desktop at SDR white | done |
| Relative mouse by default; "Optimize mouse for remote desktop" sends positions | Relative by default; positions with the mode toggle | done |
| Mouse mode toggle (Ctrl-Alt-Shift-M) | Same | done |
| Swap mouse buttons, reverse scrolling | Same, Input settings (imported from Moonlight) | done |
| Command as the Windows key (option) | Same; always Command on a Mac host | done |
| Paste the clipboard (Ctrl-Alt-Shift-V) | Same, Unicode, survives 10% burst loss | done |
| (Moonlight has none) Clipboard shared both ways | Text, images, files and folders; password managers' copies stay put ([usage.md](usage.md#sharing-the-clipboard)) | done, beyond parity |
| Controllers, several, with rumble | Same (virtual Xbox 360 pads); rumble reaches the pad ~7 ms after the game sets it | done (Xbox Wireless Controller) |
| Controller's Xbox button | Reaches the host as Guide (read from the pad's HID reports, as Moonlight does); macOS 26 opens its Games overlay too, as with Moonlight | done |
| Controller as a mouse (hold Start) | Same | done |
| PlayStation controllers as DualShock 4 / DualSense pads, with motion and touchpad | Not built: Xbox 360 pads only | no |
| Windows host input through a virtual HID driver (Sunshine's libvirtualhid) | `SendInput` and ViGEmBus, as Sunshine without its driver: the driver needs a paid, source-available licence, which a GPL-3.0 project cannot ship | n/a |

## Host

| Sunshine (and Apollo) | Pong | Status |
|---|---|---|
| Streams a physical display, changing its mode to the client's (Sunshine); a virtual display at the client's mode (Apollo) | A virtual display at the client's mode on Windows (SudoVDA, Apollo's driver) and macOS (`CGVirtualDisplay`), primary, the host's monitors off for the session (Sunshine's "deactivate other displays") | done |
| Apps: Desktop, Steam Big Picture (closed at the end) | Same | done |
| Apps of your own, with commands before and after | Not built | no |
| Runs at boot, streams UAC prompts and the lock screen | Windows service; sessions also start while the host is locked (after sleep) | done |
| Windows asked for streaming: 0.5-1 ms timer, DWM by MMCSS, Wi-Fi in media-streaming mode, Mouse Keys without a mouse | Same (1 ms), undone when the session ends | built |
| Web UI | Pairing, clients, settings, logs, ending a session; and Pong's own window | done |
| Host shutting down or restarting mid-stream | Ping says so at once | done |
| Host software restarting mid-stream | Ping resumes by itself (~10 s) | done |
| Streams with its monitor asleep, off or unplugged | Yes; sessions end in under 0.1 s with no monitor (6 of 6) | done |
| AMD (AMF) and Intel (QuickSync) encoders on Windows, software encoding | Not built: NVENC only on Windows (FFmpeg's VA-API, NVENC or x264 on Linux) | no |
| Network priority on Wi-Fi (QoS) | Not possible on Pong's dual-stack socket | no |
| A macOS host: AVFoundation capture of a physical display, sound from a loopback device (BlackHole), gamepads through a licensed driver | ScreenCaptureKit of a virtual display at the client's size, HDR, 5.1/7.1 through a tap; no gamepads (Apple grants the virtual HID entitlement on request) | done: see [platforms/macos.md](platforms/macos.md) |
| Linux: KMS, wlroots and KWin capture, uinput keyboard, mouse and pads, Vulkan encoding | X11 (MIT-SHM) and the desktop portal (PipeWire), XTest and the portal's input; no pads | no (beyond X11 and the portal) |

## Beyond Moonlight + Sunshine

- Clients and hosts on macOS, Windows and Linux, from one code base.
- Everything in one post-quantum tunnel (WireGuard with ML-KEM), paired
  with a hybrid post-quantum PIN exchange.
- Reaching the host from anywhere without port forwarding or a VPN.
- A virtual display at the client's exact mode on Windows and macOS hosts.
- HDR from a Mac host.
- Held keys repeat at the client keyboard's own pace.
- The clipboard shared both ways, files included.
- AI agents as clients of their own, held to the host's rules
  ([ai-agents.md](ai-agents.md)).

## Not built

PlayStation pads with motion and touchpad, apps of
your own, AMD and Intel encoders on a Windows host, and Linux capture and
input beyond X11 and the portal. Phones, tablets and TVs are out of scope:
pingpong is for desktops.
