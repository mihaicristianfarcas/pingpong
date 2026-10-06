# Architecture

pingpong streams a computer's desktop — picture, sound, keyboard, mouse,
controllers and clipboard — to another computer, the way Moonlight and
Sunshine do, with every byte inside one post-quantum WireGuard tunnel.
**Ping** is the client, **Pong** the host.

This page is the map: what runs where, how a frame gets from the host's
screen to the client's, and which crate does what. Why each piece is the way
it is, and what was measured on the way, is in the
[design history](design/README.md) and in the code's comments.

## The pieces

```
 Ping (client)                                   Pong (host)
 ─────────────                                   ───────────
 ping-app  (GPUI window: hosts,                  pong  (service / app / user unit)
            pairing, settings, agents)             ├─ web UI + API (HTTPS, 47802)
 ping      (CLI)                                   ├─ pairing (TCP, 47801) + mDNS
   │                                               ├─ presence (DHT, STUN, UPnP)
   └─ ping-core ───── one UDP socket ─────────────┤
        stream: net thread, input thread           └─ session: display → capture →
        platform: window, decoder, renderer           encode → FEC → pace → tunnel
                                                   pong-app (Pong's own window)
```

- One **tunnel** per paired client, pq-boringtun (WireGuard with static
  ML-KEM authentication) carried in-process on one UDP socket per side — no
  TUN device, no admin rights for it (`pingpong-transport`).
- Inside the tunnel, pingpong's own **wire format**: a 20-byte header that
  *is* the inner IPv4 header, and four kinds of payload — video, audio,
  input, control (`pingpong-proto`).
- **One session at a time** per host, on a display made for its client's
  mode. A new client takes over or is refused (per the host's settings); AI
  agents follow stricter rules ([ai-agents.md](ai-agents.md)).

### Crates

| Crate | What it is |
|---|---|
| `pingpong-proto` | The wire protocol, with no I/O: headers, packetizing and Reed-Solomon FEC, reassembly, the frame gate (loss recovery), control, input, audio and clipboard messages |
| `pingpong-transport` | The tunnel endpoint: pq-boringtun per peer on one UDP socket, roaming, segmented handshakes, batched sends and receives |
| `pingpong-pairing` | PIN pairing (SPAKE2 + ML-KEM-768), mDNS discovery, Wake-on-LAN |
| `pingpong-nat` | Reaching a host from anywhere: STUN, sealed rendezvous records on the Mainline DHT, hole punching, UPnP / NAT-PMP port mapping |
| `pingpong-display` | Virtual displays: SudoVDA on Windows, `CGVirtualDisplay` on macOS |
| `pingpong-capture` | Screen capture: Desktop Duplication (Windows), ScreenCaptureKit (macOS), MIT-SHM and PipeWire (Linux) |
| `pingpong-encode` | Video encoders: NVENC with a D3D11 colour converter (Windows), VideoToolbox (macOS), FFmpeg (Linux) |
| `pingpong-decode` | Video decoders: VideoToolbox (macOS), FFmpeg with D3D11VA (Windows) or VA-API (Linux) |
| `pingpong-audio` | Opus, the client's jitter buffer and playback, WASAPI capture on Windows |
| `pingpong-input` | Input injection on the host: `SendInput`, CoreGraphics events, XTest |
| `pingpong-clipboard` | Sharing the clipboard both ways during a session |
| `pingpong-ui` | The design system both windows use (GPUI): theme, icons, controls, Markdown; and what they need of the desktop: menus, the tray icon, one running copy, starting at login |
| `pingpong-update` | Whether a newer release, or newer commits on `main`, exist: GitHub's public API, once a day |
| `ping-core` | The client: session, stream, input, statistics, the host store; per-platform window, decoder and renderer; the `ping` CLI |
| `ping-app` | Ping's window (GPUI), and `Ping mcp` |
| `ping-agent` | Computer use: a headless session, the actions, the MCP server, the agent runners; the `ping-agent` CLI |
| `pong` | The host: sessions, the video and audio pipelines, pairing, presence, the web UI, the Windows service |
| `pong-app` | Pong's window and tray icon (GPUI), a client of the host's web API on localhost |

`vendor/` holds two tiny no-op crates GPUI names but does not use (see the
workspace `Cargo.toml`); `spikes/` holds the early experiments
([spikes/README.md](../spikes/README.md)).

## One client, one host, on every desktop

Almost all of a streaming client is the same everywhere — the tunnel,
negotiation, loss recovery, input batching and redundancy, the audio jitter
buffer, statistics, pairing, discovery, Wake-on-LAN, the internet path, the
host list and the settings. What differs is a thin layer that has to be
native to be good: how a window takes the keyboard and pointer, how a decoded
frame reaches the screen without a copy or a queue, how a controller is read.
So there is one client and one host, with a platform layer each:

| | macOS | Windows | Linux |
|---|---|---|---|
| Client window and input | AppKit (`ping_core::mac`) | Win32 (`ping_core::win`) | winit, X11 or Wayland (`ping_core::linux`) |
| Client decode | VideoToolbox | FFmpeg + D3D11VA | FFmpeg + VA-API, or software |
| Client present | Metal | Direct3D 11 | wgpu (Vulkan or GL) |
| Client controllers | GameController + IOHID | XInput | gilrs (evdev) |
| Client audio out | CoreAudio | WASAPI | cpal |
| Host display | `CGVirtualDisplay` at the client's mode | SudoVDA at the client's mode | the screen as it is, scaled |
| Host capture | ScreenCaptureKit | Desktop Duplication | MIT-SHM (X11), PipeWire through the desktop portal (Wayland) |
| Host encode | VideoToolbox | NVENC | FFmpeg: VA-API, NVENC or x264 |
| Host input | CoreGraphics events | `SendInput`, ViGEm pads | XTest (X11), the RemoteDesktop portal (Wayland) |
| Host sound | ScreenCaptureKit | WASAPI loopback of a virtual sink | PulseAudio / PipeWire monitor |

The host's platform layer is `pong/src/platform.rs` (Windows),
`pong/src/mac/` and `pong/src/linux/`; they are compiled under one module
name, so the session code above them is the same on every host. Details and
limits per system are in [platforms/](platforms/).

## Processes and threads

**Pong.** On Windows a service (`PongService`, LocalSystem) launches
`pong host` into the console session with a SYSTEM token: only SYSTEM can
open the secure desktop, so only it can capture and control a UAC prompt or
the lock screen (the model Sunshine uses). On macOS it is `Pong.app`, started
at login; on Linux a systemd user unit in the desktop session. The host
process runs:

| Thread | Does |
|---|---|
| main (`serve`) | Receives every datagram: control and input dispatched at once, session requests handed to the session thread |
| `session` | The session lifecycle (`pong/src/session.rs`): negotiate, set up the display, start the pipelines, ack, watch the client, tear down |
| `video-encode` | Capture → colour conversion → encode, at the negotiated rate (`pong/src/video.rs`, `pong/src/unix/video.rs`) |
| `video-send` | Packetize with FEC and pace onto the tunnel (`pong/src/sender.rs`) |
| `audio` | Capture → Opus → send, with parity |
| `pairing`, `web`, `presence` | PIN pairing, the web UI and API, the internet path |

**Ping.** The app and the CLI start the same stream (`ping_core::session`):

| Thread | Does |
|---|---|
| `ping-net` | Receive → reassemble → frame gate → decode; control; timers (`ping_core::stream::net`) |
| `ping-input` | Batch and send input, with redundancy (`ping_core::input`) |
| `ping-render` / `ping-decode` / window | The platform's presentation |

## A frame, from the host's screen to the client's

1. **Display.** The session gets a display at exactly the client's mode —
   a virtual one on Windows and macOS, made primary, the host's own
   monitors off (unless asked otherwise) so windows open where the client
   can see them.
2. **Capture**, paced to the negotiated frame rate — the client display's
   own, to the millihertz (59.94 Hz is not 60), as Sunshine takes
   Moonlight's: never faster, and a still desktop re-encoded at max(fps/5,
   10) frames a second so it keeps sharpening. This is Sunshine's capture
   loop, re-encoding less often than its half rate (`pong/src/pipeline.rs`
   says why).
3. **Colour conversion** on Windows: BGRA → NV12 in D3D11 shaders,
   BT.709 limited range, chroma sited left, the colour description written
   into the stream (`pingpong-encode/src/convert.rs`). For HDR, the FP16
   desktop → BT.2020 PQ in P010; for 4:4:4, packed AYUV.
4. **Encode**, configured as Sunshine configures NVENC: CBR with a
   single-frame VBV, no B-frames, an endless GOP, a few reference frames so
   a loss can be repaired by reference invalidation instead of a keyframe.
5. **Packetize and FEC.** The frame (behind a 4-byte prefix carrying the
   host's processing time) is split into shards — 1180 bytes on a 1280-byte
   path, 1400 on the local network — and Reed-Solomon parity is added per
   block, per frame: recovery never waits for a later frame.
6. **Pace.** Datagrams leave in 1 ms groups under the pacing rate, as
   Sunshine sends, not as one burst that overflows a Wi-Fi queue; batched into
   a few system calls (USO, GSO, `sendmsg_x`).
7. **Tunnel.** Each datagram is encrypted and sent to the client, then to
   anyone watching an agent's session (the same encode, a second recipient).
8. **Reassemble.** The client collects shards per frame and recovers lost
   ones from parity (`pingpong-proto/src/reassemble.rs`).
9. **Frame gate.** Moonlight's rule: after a lost frame, nothing is decoded
   until a frame that does not depend on it arrives. The client asks the
   host to invalidate the lost frames as references (the next frame is a
   *recovery* frame) or, failing that, for an IDR
   (`pingpong-proto/src/video.rs`).
10. **Decode and present.** Hardware decode, then the newest frame to the
    screen at once — or, with frame pacing on, Moonlight's pacer: one frame
    per display refresh. An HDR stream is drawn PQ-encoded into an HDR
    layer with the host's metadata (on a Mac with an HDR display).

The client reports loss and round trip once a second; the host adapts the
bitrate to congestion and the FEC to the link's own loss
(`pong/src/bitrate.rs`).

### Why the video path looks like Sunshine's

Each of these was a visible artefact until it was done the Sunshine and
Moonlight way, first learnt from Apollo, Sunshine's fork (the full table,
with the source it was checked against, is in
[design/v3-design.md](design/v3-design.md)):

| Choice | Without it |
|---|---|
| Decode nothing after a loss until a recovery frame or IDR | smeared blocks, regions that never heal |
| Own colour conversion, colour description in the stream | soft text with coloured fringes |
| Single-frame VBV, reference frames kept for invalidation | quality swings, keyframes for every loss |
| Desktop Duplication as SYSTEM, following the input desktop | UAC prompts and the lock screen invisible, input dead |
| Re-encode a still desktop | a still image stuck at the quality motion left it |
| Paced, batched sends | burst loss on Wi-Fi |

## The wire

Every pingpong packet is one tunnel datagram whose first 20 bytes are the
inner IPv4 header boringtun expects; boringtun checks only three of them, so
the rest carry pingpong's own fields (`pingpong-proto/src/header.rs`, the
only file that knows byte offsets):

```
byte  0     0x45            version = 4 (required by the tunnel)
byte  1     flags           keyframe · frame_end · kind (2 bits) · recovery · lan_shards
bytes 2-3   total_len       big-endian, exact (required by the tunnel)
bytes 4-5   fragment_idx
byte  6     data_shards
byte  7     parity_shards
byte  8     fec_block_idx
bytes 9-11  frame_len
bytes 12-15 capture_ts_us
bytes 16-19 frame_id
```

| Kind | Carries | Reliability |
|---|---|---|
| Video | Frame shards and parity | Reed-Solomon per frame; lost frames repaired by invalidation or IDR |
| Audio | One 5 ms Opus packet per datagram; two parity datagrams per four | Reed-Solomon 4+2, as Sunshine; concealment beyond that |
| Input | The newest events, and the ones before them (up to 8) | Unreliable but redundant: every packet repeats recent events, the host drops what it has seen |
| Control | Session setup, loss reports, recovery requests, cursor state, pings, controllers, clipboard | Retransmitted by the sender where it matters (`SessionStart`), otherwise naturally repeated |

Input is sent unreliably on purpose: a retransmission would hold every later
event back by a round trip. Keys travel as physical scancodes (the host's
layout applies, as games expect); the mouse as relative motion, or
absolute stream pixels when the user switches to positions. The host draws
its pointer into the picture, as Sunshine does, so the pointer the client
sees is the host's own.

## Pairing and trust

No keys are copied by hand:

1. Ping finds Pong by mDNS (`_pingpong._udp`), or is given an address.
2. Ping shows a PIN; you type it into Pong's window or web UI.
3. SPAKE2 over the PIN, with an ML-KEM-768 exchange beside it, gives both
   sides a key that only they have — the pairing is hybrid, like the tunnel,
   so a recording of it stays closed to a future quantum computer. Each side
   then sends its tunnel keys (X25519 and ML-KEM-768) under it, with the
   keys for finding each other across the internet.

A client is then known by its tunnel key; there is nothing else to steal
from the network. See [SECURITY.md](../SECURITY.md) for the model, and
[design/v3-design.md](design/v3-design.md) §4 for why the pairing is hybrid.

What a client may do once paired is the host's to say, per client, as
Apollo does: its permissions (`pingpong_proto::permission`, kept in
`clients.toml`) say whether it may see the screen, which input it may send,
which way the clipboard goes, and whether it may start apps, take over or
watch agents. The host checks them where each thing arrives (a session
request, an input packet, a clipboard chunk) and tells the client its set
in the session's ack and when it changes ([usage.md](usage.md#what-each-device-may-do)).

## Away from home

A host with internet access on publishes a sealed record of its public
addresses on the BitTorrent Mainline DHT, readable only by devices paired
with it; a client that wants to connect publishes an intent, and both punch
through their NATs. The router is asked to forward the port too (UPnP,
NAT-PMP), where it can. No servers are involved. See
[networking.md](networking.md).

## Sessions

```
SessionStart ─▶ display at the client's mode ─▶ video pipeline ready
            ─▶ input installed ─▶ SessionAck ─▶ frames flow
SessionEnd / client silent / pipeline died ─▶ release keys ─▶ stop video
            ─▶ the platform puts the host's displays back
```

A session is acked only once its pipeline runs, so the client never waits
on a session that cannot start. A client that goes quiet keeps its session
(and its display) for 20 seconds: coming back within that resumes the
stream. After a session, the host keeps the virtual display for a minute for
a client coming back.

## Where to read next

- [platforms/](platforms/) — what each system does, and its limits.
- [networking.md](networking.md) — the internet path.
- [ai-agents.md](ai-agents.md) — AI agents as clients.
- [benchmarks.md](benchmarks.md) — what it all costs, measured.
- [design/](design/README.md) — the designs and measurements behind this.
