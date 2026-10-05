# Benchmarks

What pingpong costs, measured: micro-benchmarks of its own code paths, and
the real stream end to end, against Moonlight + Sunshine/Apollo where that
means something. The host comparisons so far were measured against Apollo,
the Sunshine fork whose virtual display Pong follows: its capture, NVENC
settings, FEC and pacing are Sunshine's. Every figure says what it was
measured on; the earliest measurements (v1, v2) are in the
[design history](design/README.md).

## Test setup

Unless a section says otherwise:

| | |
|---|---|
| Client | 14" MacBook Pro, Apple M4 Pro, macOS; its built-in display (120 Hz ProMotion), on Wi-Fi |
| Host | Windows 11 25H2 PC, NVIDIA RTX 3070 Ti, SudoVDA; on the same LAN (Wi-Fi at first, then 1 Gbit/s Ethernet) |
| Stream | 3024x1890 (the MacBook's display below the notch) at 120 fps, 100 Mbit/s, HEVC, 7.1 audio: a demanding Moonlight setting |
| Content | a full-screen scene of random noise at 60 fps, which fills the bitrate; or moving boxes, for decode damage |

## How to measure

- **Micro-benchmarks** (Criterion, release profile):

  ```sh
  cargo bench -p pingpong-transport   # pq-boringtun handshake and data plane
  cargo bench -p pingpong-proto       # packetize, FEC, reassembly, audio
  cargo bench -p pingpong-decode      # the Annex-B split before VideoToolbox
  ```

- **The stream**: the statistics overlay (Ctrl+Alt+Shift+S, or
  `ping stream NAME --stats`) shows the same figures as Moonlight's, plus the
  host's capture to the client's screen. For a log of them, one line a
  second: `RUST_LOG=info,ping_core::stats=debug ping stream NAME ...`. The
  host logs a line a second too (`pong.log`: frames, capture to encoded,
  Mbit/s, recoveries).
- **Host capture to the client's screen** is measured directly: the two
  machines' clocks are related through the ping/pong exchange the stream
  already runs (every 500 ms; as in NTP, the sample with the shortest round
  trip of the last 16 sets the offset).
- **Impairing the link** without network tools: see
  [development.md](development.md#test-hooks) (`PING_TEST_LOSS`,
  `PINGPONG_TEST_WIRE_LOSS`, ...).

Micro-benchmark numbers below: Apple M4 Pro, rustc 1.98.1, 2026-09-27.

## pq-boringtun (static ML-KEM authentication, 1280-byte path)

In memory, no sockets: exactly the per-peer state an `Endpoint` keeps.

| What | Time | Notes |
|---|---|---|
| Complete handshake, both sides | 364 µs | initiation is 3 datagrams, 2,600 bytes (segmented) |
| Encrypt a 1200-byte packet | 0.63 µs | 1.76 GiB/s |
| Encrypt + decrypt, 1200 bytes | 1.38 µs | 827 MiB/s |
| Encrypt + decrypt, 100 bytes (input, control) | 0.36 µs | |

At an 80 Mbit/s stream (~8,300 datagrams/s) encryption is ~5 ms of CPU per
second, about 0.5% of one core, per side. A rekey every two minutes costs
0.36 ms of CPU and, being off the data path, no stall.

## Data path

Since 2026-09-30 (the optimization pass below; the earlier figures in
brackets):

| What | 2 KB P-frame | 42 KB P-frame | 600 KB IDR |
|---|---|---|---|
| Packetize (split + RS parity) | 0.20 µs (0.44) | 5.1 µs (7.3) | 126 µs (156) |
| Reassemble, no loss | 0.08 µs (0.48) | 1.2 µs (5.6) | 25 µs (88) |
| Reassemble, one datagram lost per FEC block | 101 µs (102) | 119 µs (126) | 675 µs (763) |

`cargo bench -p pingpong-decode` measures the Annex-B split every frame
goes through before VideoToolbox: 2.5 µs for a 120 KB frame (a SIMD
start-code search), where the byte-at-a-time loop took 33 µs.

42 KB is a busy 1080p60 frame at 20 Mbit/s; 600 KB a 4K keyframe.

Audio: packetizing a 5 ms Opus packet costs 95 ns; recovering a block of four
with one lost, 101 µs.

### What the numbers say

- **Recovery has a fixed cost of ~0.1–0.2 ms.** reed-solomon-simd is Leopard
  RS over GF(2^16): locating erasures takes a transform over the whole field,
  whatever the block size. For video that is small beside a frame interval
  (8.3 ms at 120 fps). For 4+2 audio blocks a GF(2^8) Cauchy code, which is what
  Sunshine and Moonlight use (nanors), would take microseconds; worth doing if
  recovery ever shows up in a profile.
- **The first encoder pays ~6 ms of table setup.** Host and client now warm the
  FEC tables at startup (`fec::warm_up`), so a session's first keyframe does not.

## Decode to glass (client presentation)

Measured 2026-09-27 on the MacBook's built-in 60 Hz display, 1080p60 HEVC
over the LAN. "Decode to glass" is from the decoder's output to the time
Metal reports the drawable reached the display (`presentedTime`), so it
includes the compositor.

| Mode | Content | Decode to glass | Frames shown |
|---|---|---|---|
| Full screen, present on decode (default) | 60 fps noise | 7.2 ms | 57 of 61 |
| Full screen, display link (the first frame pacing) | 60 fps noise | 28.8 ms | 60 of 60 |
| Full screen, present on decode | 31 fps noise | 6.3 ms | 31 of 31 |
| Full screen, display link | 31 fps noise | 20.1 ms | 28 of 31 |
| Window, present on decode | 31 fps noise | 21.1 ms | 30 of 31 |

The display link hands out its drawable a refresh or more before the refresh
it targets, so a frame decoded just after the callback waits almost two. In a
window it aims three refreshes ahead (50–60 ms). Presenting on decode, as
Moonlight does by default, loses a frame now and then when two land in one
refresh; frame pacing is an opt-in setting, as in Moonlight.

The rest of the pipeline in the same runs, from the overlay: host capture to
encoded 2.2–2.6 ms, in flight about half the ~9 ms Wi-Fi round trip, first
packet to complete frame 1–1.6 ms, decode 1.5–2 ms.

## Against Moonlight + Apollo

Apollo streams with Sunshine's video path and adds the virtual display at
the client's mode, which is what made it the like-for-like host to measure
Pong against (Sunshine itself streams a physical display). Not measured
yet: Sunshine's own newer settings (split-frame NVENC on GPUs with two
encoders), which do not apply to this host's single-encoder RTX 3070 Ti.

2026-09-27, the same settings on both: 3024x1890 at 120 fps,
100 Mbit/s, HEVC, 7.1 audio, V-Sync and frame pacing on; 60 fps noise on
the host; the Mac on Wi-Fi to the host on the LAN (the host on Wi-Fi too).
Moonlight 5.x against Apollo's "Virtual Display" app, its end-of-stream
"Global video stats"; Ping against Pong, the per-second statistics
averaged over 30 s. The Mac's display was asleep, so neither side's
presentation times are what a person watching would see; they compare
with each other.

| | Moonlight + Apollo | Ping + Pong |
|---|---|---|
| Frames received / decoded | 110.9 fps | 117.6 fps |
| Frames shown | 110.0 fps | 115.6 fps |
| Host processing | 5.6 ms | 5.0 ms |
| Decode | 2.32 ms | 1.6-2.0 ms |
| Network round trip | 9 ms | 13.6-17.9 ms |
| Pacing and presentation | 5.4 ms queue + 1.3 ms render (to its present call) | 12.8 ms (decode to glass) |

The round trips are not measured alike. Moonlight's is ENet's smoothed
estimate on its control channel ("variance: 0 ms"), while the same run logs
single control messages at 109 ms. Ping's is each second's raw ping
through the tunnel, queued behind the video. A second run the same evening:
Moonlight 105.4 fps received, 0.45% of frames lost and 0.79% dropped for
jitter, "9 ms"; Ping 108.6 fps shown, round trip 24 ms median, 58 ms p90
under 113 Mbit/s of noise and 10.6 ms with a light scene (8.5 Mbit/s).

Not done: Sunshine's network priority, a qWAVE flow of type AudioVideo
(WMM's video category on Wi-Fi). qWAVE won't take Pong's dual-stack socket
with an IPv4 client, neither IPv4-mapped (ERROR_NOT_FOUND) nor as IPv4
(ERROR_INVALID_PARAMETER). Sunshine's sockets are IPv4-only by default.
It would take an IPv4 socket of its own, or a system QoS policy on the host.

Ping's frame pacing first ran on a `CAMetalDisplayLink`: 20.3 ms decode to
glass here, because the link hands out its drawable a refresh or more
ahead of the one it targets. It is now Moonlight's pacer -- a
`CVDisplayLink` tick, on which the oldest queued frame is drawn and shown
at the next refresh, a standing queue skipped to its newest -- at 12.8 ms
and 115.6 of 117.6 frames shown (showing only the newest frame per
refresh dropped to 105).

## The host on Ethernet (2026-09-28)

The host on 1 Gbit/s Ethernet, the Mac on Wi-Fi, 3024x1890@120, 100 Mbit/s,
7.1 audio, 60 fps noise, the Mac's display awake, full screen; per-second
statistics over 20-40 s:

| | Both on Wi-Fi (2026-09-27) | Host on Ethernet |
|---|---|---|
| Loss | not recorded alike | 0.00% at 113 Mbit/s |
| Round trip under load | 24 ms median, 58 ms p90 | 6-10 ms median, 7-22 ms p90 |
| Frames received | 117.6 fps | 118.2-118.8 fps |

With the display awake, presentation was slower than the asleep-display
runs above had shown: decode to glass 28.9 ms presenting on decode, 27.7
ms with frame pacing. At 120 fps on the 120 Hz panel, a burst filled the
layer's three drawables and nothing drained them, so every frame was shown
two refreshes late (at 60 fps, which the panel drains, it was 10 ms). Ping
now draws a frame only when fewer than one (with pacing, two) wait for a
refresh:

| At 120 fps | Before | After | Frames shown |
|---|---|---|---|
| Present on decode: decode to glass | 28.9 ms | 10.1-10.3 ms | 115.6-116.9 of 118.5 |
| Present on decode: capture to glass | 41.9 ms | 22.3-22.6 ms | |
| Frame pacing: decode to glass | 27.7 ms | 16.9-17.5 ms | 116.2 of 118.5 |
| Frame pacing: capture to glass | 39.9 ms | 29.5-29.7 ms | |

A limit of one with pacing showed 101 fps (a refresh tick comes just before
the previous frame reports being on the glass); two without it, 14.4 ms.

In a window the window server composites each frame, up to two refreshes
later, so the limit applies only full screen. 3024x1890 with 60 fps noise,
present on decode:

| In a window | Limit 1 | Limit 2 | Layer's 3 (kept) |
|---|---|---|---|
| 60 fps: shown, decode to glass | 40 of 60 (1080p, 34 ms) | 60 of 60, 22.8 ms | 60 of 60, 21.6 ms |
| 120 fps: shown, decode to glass | | 80 of 118, 31 ms | 118 of 119, 24-26 ms |

Controller rumble, the game's XInputSetState to Ping's haptics call: ~7 ms
(the two machines' clocks differ by ~207 ms, corrected with time.apple.com).

## Capture to glass

The overlay's last line, "Host capture to screen", measures it directly:
from the host picking up a captured frame to Metal reporting it on the Mac's
display. The two machines' clocks are related through the Ping/Pong exchange
the stream already runs (every 500 ms): a Pong's header carries the host's
clock when it replied, and as in NTP the sample with the shortest round trip
of the last 16 sets the offset.

Measured 2026-09-27, 1080p60 HEVC at 20 Mbit/s, 60 fps noise, full screen
(present on decode), Mac on Wi-Fi to the host on the LAN: **17–24 ms per
second, typically 18 ms**, which matches the sum of the stages above. Peaks of
80–200 ms are the Mac's Wi-Fi stalling (below).

Not included: the host compositor's time from an application's present to
the capture (DWM), and the Mac panel's scanout and pixel response. Those
need a photodiode or a 240 fps camera on both screens.

## Long sessions and loss

35 minutes, 1080p60 moving scene with audio, every received datagram
(handshakes included) dropped at 2% in bursts of 3 and handshake datagrams
at a further 30% (`PINGPONG_TEST_WIRE_LOSS=2:3 PINGPONG_TEST_HANDSHAKE_LOSS=30`
on the client), so each of the ~17 rekeys had to survive lost messages:

- No session interruptions: no IDR requests, no renegotiation, no lost contact.
  (Keys are rejected after 3 minutes, so the session outliving that is itself
  the proof that every rekey landed.)
- 123,599 of 125,460 frames shown (98.5%); 339 frames lost to the injected
  loss, each repaired by a reference invalidation. 85 of 2,091 seconds showed
  fewer than 55 frames.
- Audio: 0.9% of packets concealed.

The test Mac's Wi-Fi on its network stalls for 50–200 ms every 20–30 s, loss or
not: audio packets the host sent 20 ms apart arrive up to 205 ms apart, while
the client's receive loop never spends more than 1–3 ms on a datagram. Each
such stall empties the audio buffer (about 2 underruns a minute in the long
run). AWDL (AirDrop, Continuity) was up on that Mac and is a common cause of
exactly this pattern; not yet confirmed here by turning it off
(`sudo ifconfig awdl0 down`, until the next reboot).

## Starting a stream

From launching the client to the first frame, host monitor asleep, Mac on
Wi-Fi. Ping: 15 consecutive start/stop cycles (10 at 3024×1890@120, 100 Mbps,
7.1 audio; 5 at 1080p60), Apollo stopped. Moonlight: one start of the Desktop
app at the same settings.

| | Launch → first frame |
|---|---|
| Ping → Pong | 5.6–7.0 s, typically 6.1 s; 15 of 15 started |
| Moonlight → Apollo | ~5 s |

Where Pong's time goes (3024×1890@120):

| Step | ms |
|---|---|
| Tunnel up | 11 |
| Virtual display added | 2 |
| Windows puts it on the desktop | 3,880 |
| Mode forced, made primary | 210 |
| Other displays turned off | 600 |
| Capture and encoder up | 320 |

Windows' 3.9 s is the first display-configuration call after the monitor
arrives, blocked until Windows has brought it up. Apollo's monitor takes ~3 s
to become its desktop on this host too.

It is the host's own monitor that costs it. 2026-09-28, the host on
Ethernet, from the tunnel up to the session started:

| The host's monitor | Virtual display on the desktop | Tunnel → session started |
|---|---|---|
| Awake | 153-181 ms | 0.87-1.5 s (13 starts) |
| Switched off at its button (still connected) | 3,872-3,903 ms | 5.2-6.0 s |
| Unplugged (headless) | 327-372 ms | 0.62-0.79 s (10 starts) |

Asleep or switched off, the monitor is still on the cable, and Windows
spends ~3.7 s on it whenever the arrangement changes.

That wait is also why every session failed once Apollo was stopped: SudoVDA
removes a monitor 3 s after the last ping. Pong pinged only between display
calls, and Apollo's own pings had kept the monitor alive. The keepalive now
starts before the monitor is added.

## Optimization pass (2026-09-30)

Ping (Mac, Wi-Fi) against Pong (the Windows host, Ethernet): 3024x1890 at
120 fps, 100 Mbit/s, HEVC, full screen, 60 fps noise on the host, about 114
Mbit/s on the wire. Each figure is the
average of a run's middle 24-26 seconds; A and B were run back to back.

| Change | Measured |
|---|---|
| Client: receives batched (`recvmsg_x`, 32 at a time) | first packet to complete frame 2.4-2.6 ms -> 1.1-1.2 ms; the same with batches of 8 or 4 as with none: Wi-Fi hands over bursts |
| Client: full screen asked again when macOS refuses it (a stream started just as another left its Space) | such sessions ran in a window, decode to glass 22-28 ms; now full screen, 8-10 ms |
| Client: SIMD Annex-B split, the AVCC sample written straight into the block buffer | decode 2.36-2.51 -> 2.24-2.36 ms |
| Client: reassembly into reused buffers (no allocation per datagram) | see the table above |
| Host: sends batched, 16 datagrams per call (USO on Windows, GSO on Linux, `sendmsg_x` on macOS); packetizing into one reused buffer | pong.exe 24-25% of a core -> 13-18%, frame delivery unchanged (1.6 ms) |
| Host: 64 datagrams per call (a whole pacing group) | the same CPU, but frames 1 ms later: a call that large held the sender until it was all on the wire |
| LAN-sized shards (1400-byte payloads on the local network, as Moonlight's 1392) | the client's receive thread 33% -> 31% of a core at the same rate; latency unchanged |
| Client: user-interactive QoS on the stream's threads | no change measured (the receive thread still runs mostly on efficiency cores); real-time scheduling did no better |
| Client: socket marked for Wi-Fi's voice queue (`SO_NET_SERVICE_TYPE`, as Moonlight) | no change in round trip measured on the test network |

A five-minute run with all of it: 117.9 fps received, 111.7 shown, 2 frames
lost (both recovered), memory flat.

What the client's CPU goes to at this rate (Instruments, 8 s): the receive
thread about 31% of a core, nearly all of it in the kernel (`recvmsg_x` and
what comes with waking); ChaCha20-Poly1305 1%; Metal's command submission
3%; drawing 2%; VideoToolbox's callbacks 2%; audio 1%. Fewer, larger
datagrams saved less than their count suggests: the kernel's cost follows
the Wi-Fi bursts more than the datagrams.

Idle: Pong's session thread ticks every 100 ms between sessions (was 8 ms);
Ping polls its host list every 30 s while in the background (every 4 s in
front): Ping.app hidden went from about 160 wakeups a second to about 90.

Switches for diagnosis: `PINGPONG_SEND_BATCH=N` (0: one datagram per call),
`PINGPONG_RECV_BATCH=N` (0: one per call), `PINGPONG_LAN_SHARDS=0` (client:
path-sized datagrams on the LAN too), `PINGPONG_SERVICE_CLASS=0` (see
[cli.md](cli.md#environment-variables)).

## Ping 0.8.0 against 0.7.0 (2026-10-05)

Whether the renderer's HDR and 4:4:4 paths cost an SDR stream anything, and
what the Windows host's streaming set-up (full GPU power, a 1 ms timer, DWM
under MMCSS, Wi-Fi in media-streaming mode) does to host processing. Ping
(Mac, Wi-Fi) against Pong 0.8.0 (the Windows host, Wi-Fi), through a VPN
overlay rather than the LAN: 3024x1890 at 120 fps, 100 Mbit/s, HEVC, 7.1
audio, frame pacing on, full screen, 60 fps noise on the host. Two pairs of
runs, 0.7.0 then 0.8.0 and then the other way round; each figure is a run's
62 s under load.

| | Ping 0.7.0 | Ping 0.8.0 |
|---|---|---|
| Frames received | 116.8-118.4 fps | 117.3-117.7 fps |
| Frames shown | 111.6 fps | 110.9-111.6 fps |
| Decode | 1.49-1.58 ms | 1.52-1.58 ms |
| Decode to glass (median) | 15.1-16.6 ms | 16.5 ms |
| Host processing | 5.1-5.2 ms | 5.2 ms |

The new paths cost the SDR stream nothing measurable. Host processing is
5.1-5.2 ms against 5.0 ms on 2026-09-27 before the streaming set-up: no gain
at this load, where the GPU's clocks are up anyway (full power is for light
scenes, which this did not measure). Fewer frames were shown than on
2026-09-28 (116.2 of 118.5) with both versions alike: the VPN path delivers
frames less evenly. Not measured: round trip and loss over the LAN.
