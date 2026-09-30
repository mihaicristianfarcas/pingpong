# pingpong v3 — Ping ↔ Pong, Moonlight ↔ Apollo parity

> **Historical record.** The design of the Moonlight/Apollo parity phase,
> kept as written. The current architecture is in
> [../architecture.md](../architecture.md). See [the design history](README.md).

**Goal.** Ping (macOS client) and Pong (Windows host) behave and feel like
Moonlight ↔ Apollo, with every byte carried by one pq-boringtun tunnel
(static ML-KEM auth, byte-perfect IPv6 MTU; pinned `ebe1617`).

v1 proved the pipeline, v2 added the virtual display and input. v3 is the
product: the video path is re-implemented the way Sunshine/Apollo and Moonlight
do it (§2), the host becomes a service with a web UI, the client becomes an app,
and pairing/discovery replace hand-copied keys.

---

## 1. What changed from v2, and why

The v2 stream showed four classes of artefact: blocky smearing, fuzzy/coloured
text, tearing/stutter, and regions that never updated. Each one maps to a place
where v2's video path differs from Apollo/Moonlight. Verified against source
(Apollo `adc5c5a`, moonlight-common-c `f900dd4`, moonlight-qt `c0f62ad`):

| Stage | Apollo / Moonlight | v2 | Artefact |
|---|---|---|---|
| Loss recovery | `VideoDepacketizer.c`: after any loss nothing reaches the decoder until a recovery frame; host invalidates references (RFI, `nvEncInvalidateRefFrames`) or sends an IDR; the request waits for the next fully received frame | P-frames after a loss are decoded against a missing reference; a keyframe is requested only after 500 ms with *no decode*, which never happens because corrupt frames decode "fine" | smearing, stale regions |
| Presentation | `vt_metal.mm`: `displaySyncEnabled = vsync`, `CAMetalDisplayLink` (`preferredFrameLatency = 1`), one-slot latest-frame mailbox | `displaySyncEnabled = false`, present on arrival | tearing, stutter |
| Colour conversion | own D3D11 shaders: BGRA → NV12, BT.709, left-cosited chroma from two bilinear taps, VUI colour description + chroma location written into the bitstream | NVENC's internal ARGB → YUV, no VUI | soft / fringed text |
| Rate control | CBR, **single-frame VBV** (`bitrate / fps`), two-pass quarter-res, 5-frame DPB with L0 = 1 (so RFI has something to fall back to), PTD on + `FORCEIDR` | CBR with preset VBV, one pass, one reference, PTD off | quality, recovery cost |
| Capture | DXGI Desktop Duplication as SYSTEM, `OpenInputDesktop` + `SetThreadDesktop` on every (re)init, so the secure desktop (UAC, lock screen) is captured | WGC as the user | UAC invisible, input dead |
| Static content | re-encode the last frame at ≥ max(fps/5, 10) fps so it keeps sharpening | 10 fps repeat | slow refinement |
| Send | batches ≤ 64 packets, paced to ~80% of 1 Gbps per 1 ms group | tight loop, spin on `WouldBlock` | burst loss on Wi-Fi |

Everything else v2 built stays: the 20-byte header overlaid on IPv4, per-frame
Reed-Solomon blocks, SudoVDA control, the scancode tables, the unreliable-but-
self-healing input channel, the client-drawn cursor with host cursor state.

## 2. Processes

### Pong (Windows)

```
PongService (LocalSystem, auto-start)          ─ pong.exe service
  └─ launches, in the active console session, with a SYSTEM token:
     pong.exe host                             ─ everything below
        ├─ UDP 47800  pq-boringtun endpoint (all paired clients)
        ├─ TCP 47801  pairing (SPAKE2 PIN + ML-KEM-768) + host info
        ├─ TCP 47802  web UI (HTTPS, self-signed, login)
        ├─ mDNS       _pingpong._udp
        └─ session    VDD → DDA → shader → NVENC → FEC → pacer → tunnel
                      WASAPI loopback → Opus → tunnel
                      tunnel → SendInput / ViGEm
```

Why a SYSTEM token in the user's session (the Sunshine model): only a SYSTEM
process can open the Winlogon desktop, so only it can capture a UAC prompt or
the lock screen and inject input into it. The service relaunches the host when
the console session changes or the host exits.

State lives in `C:\ProgramData\Pong\` (config, identity, paired clients, logs).

### Ping (macOS)

```
Ping.app (SwiftUI)          host list, pairing sheet, settings, stream window
  └─ libping_core.a (Rust)  discovery, pairing, tunnel, session, depacketizer,
                            VideoToolbox, Metal display link, Opus + CoreAudio,
                            input encoding, stats overlay
```

`ping` (a winit dev binary over the same core) stays for automated testing.

## 3. Wire

One tunnel per paired client. Inner packets keep v1's 20-byte header; `kind`
selects video / audio / input / control.

- **Video**: per-frame FEC blocks as v1. Header flag bit 4 = *recovery point*
  (first frame after reference invalidation). Frame ids are consecutive per
  session; a gap is a loss.
- **Control** (`kind=3`), client→host: `SessionStart`, `RequestIdr`,
  `InvalidateRefs{first,last}`, `LossStats`, `SessionEnd`, `Ping`;
  host→client: `SessionAck`, `CursorState`, `SessionEnd`, `Pong`, `HostStats`.
- **Input** (`kind=2`): v2's ring-redundant batches, plus gamepad state.
- **Audio** (`kind=1`): Opus, 48 kHz stereo, 5 ms packets, Opus in-band FEC.

## 4. Pairing and discovery (Moonlight's UX)

1. Ping finds Pong via mDNS (or the user types an address).
2. Ping shows a 4-digit PIN. The user enters it in Pong's web UI.
3. SPAKE2 over the PIN, and an ML-KEM-768 exchange beside it (Ping's fresh
   key, Pong's ciphertext), yield the pairing's keys together, as the tunnel
   combines X25519 and ML-KEM; each side proves it has them over the whole
   transcript, then sends its X25519 and ML-KEM-768 public keys under them.
   Pong records the client; Ping records the host. No keys are ever copied
   by hand.

   Why hybrid (pairing version 2, 2026-09-29): SPAKE2 is elliptic-curve
   cryptography, so a recording of a version-1 pairing could be opened by a
   future quantum computer, and with it the host's rendezvous secret (what
   its internet address records are sealed with). With ML-KEM's secret in the
   keys, a recording stays closed. The PIN check itself is still SPAKE2's:
   defeating it takes an attacker in the middle while the pairing happens,
   not later. There is no post-quantum PAKE standard yet (KEM-based designs
   and an IETF CFRG draft combining CPace with an ML-KEM PAKE exist), and no
   audited Rust implementation, hence the hybrid. Version 1 is refused, not
   fallen back to (an attacker could force it); pairings made with it stay
   valid.

## 5. Presentation (Ping)

The stream window is borderless fullscreen at the display's usable size below
the notch (e.g. 3024×1890); the requested mode is exactly that size, so there are
no bars. The pointer is captured and the menu bar, dock and hot corners are
suppressed while streaming. Hotkeys, as Moonlight: Ctrl+Alt+Shift+Q quit,
+S stats, +M mouse mode, +Z release capture, +X windowed/fullscreen,
+D minimise.
