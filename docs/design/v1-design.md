# pingpong v1 — Design

> **Historical record.** Kept as it was written, for the reasoning and the
> measurements behind the code; code comments cite its sections. Where it
> and the code disagree, the code and the [current docs](../README.md) are
> right. See [the design history](README.md).

**Status:** approved design, not yet implemented
**Date:** 2026-07-29
**Scope:** v1 only. Future work is enumerated in §15 and is explicitly out of scope.

A low-latency game-streaming client/server in Rust, transported over
[pq-boringtun](https://github.com/mihaicristianfarcas/pq-boringtun) — a
post-quantum WireGuard variant with segmented handshakes. Intended to replace an
Apollo (server) + Moonlight (client) setup.

---

## How to read this document

Claims here fall into two tiers, marked inline:

- **[V]** — verified against source in this repo or `pq-boringtun` at design
  time, with a file:line citation. Trust these.
- **[P]** — provisional: reasoned from experience or documentation, **not**
  measured or verified. Treat every `[P]` as a hypothesis to confirm during
  implementation. Several are load-bearing; §14 ranks the ones that matter.

Implementers: when a `[P]` claim turns out wrong, update this document rather
than working around it locally.

---

## 1. Scope

### 1.1 What v1 is

A **video-only latency spike**, end to end:

```
Windows host                                        macOS client
────────────                                        ────────────
WGC capture → CUDA NV12 → NVENC H.264 → RS FEC
    → PQ-WireGuard tunnel → UDP  ═══════════════▶  UDP → tunnel → FEC recover
                                                     → VideoToolbox → Metal
```

No audio. No input. No virtual display. No adaptive bitrate. No pairing UI.

### 1.2 Why this slice

It touches every architectural layer, so later subsystems attach to proven
seams instead of requiring surgery. It terminates in a glass-to-glass
millisecond figure directly comparable to Moonlight. If that figure is bad, it
is known in week one rather than month four.

### 1.3 Success criteria

1. 1080p60 streams from the Windows host to the macOS client over the PQ tunnel.
   — **MET [V]** 2026-07-31: `fps=60 dropped_pacer=0 dropped_queue=0`.
2. Per-stage latency is measured and attributable (§12), not merely a total.
   — **MET [V]**: host 2.81 ms, client 2.00 ms, both cross-checked against the
   sum of their stages. [v1-measurements.md](v1-measurements.md).
3. Glass-to-glass latency is within ~20% of the existing Moonlight/Apollo setup
   on the same network path. — **NOT MEASURED**, deferred: the camera comparison
   has not been run, so this criterion has no verdict either way.
4. Ten minutes of sustained streaming shows no visible artifact at rekey
   boundaries (§11.4). **This is the acceptance test unique to this project.**
   — **MET [V]** 2026-07-31: 36 000/36 000 frames across 5 rekeys, no latency
   difference inside the rekey windows. See §11.4.

### 1.4 Non-goals for v1

Audio, input, virtual display, adaptive bitrate, HEVC/AV1, multi-client,
discovery/mDNS, a pairing UI, Linux host support, path-MTU discovery.

---

## 2. Decisions and rationale

| # | Decision | Rationale |
|---|---|---|
| D1 | Embed `boringtun::noise::Tunn` as a library; no TUN device | `Tunn` is a pure userspace state machine **[V]**. No root/admin on any platform, and we control MTU and pacing directly. |
| D2 | pingpong header **is** the inner IPv4 header (§5) | Only 3 of 20 header bytes are validated on receive **[V]**. A separate shim would pay 20 B/packet for nothing. |
| D3 | Moonlight-style RTP+FEC; no WebRTC, no SRTP, no retransmission | WireGuard already authenticates every datagram. WebRTC congestion control targets ~100 ms conferencing latency, not ~10 ms gaming. |
| D4 | Reed-Solomon FEC, block = one frame | Parity for frame *N* depends only on frame *N*, so recovery never waits on future packets and adds zero latency. |
| D5 | Direct NVENC via CUDA, not ffmpeg | Zero-copy GPU path; ultra-low-latency tuning knobs (P1, no B-frames, zero reorder delay). ffmpeg's default path does a CPU readback that can cost more than the rest of the pipeline combined. |
| D6 | `cuGraphicsD3D11RegisterResource`, **not** external memory (§8) | External memory has no `CUarray` exit; graphics interop does. **[V]** Note the original rationale ("forcing a per-frame GPU copy" only on the external-memory route) was falsified — a WGC texture cannot be registered either, so both routes need one `CopyResource`. See the §8.2 correction. |
| D7 | Client presents via `objc2-metal`, not `wgpu` | The zero-copy VideoToolbox path is `CVMetalTextureCache`. `wgpu` has no clean IOSurface import **[P]**, so it would force a readback. |
| D8 | Default path MTU 1280 (IPv6 minimum) | Matches the MTU `pq-boringtun`'s segmentation was designed and tested against. 1500 is opt-in after measurement. |
| D9 | Sans-I/O core (`pingpong-proto`) | Protocol logic tested deterministically without sockets or GPU. Same shape as `Tunn` itself. |

---

## 3. Crate layout

```
pingpong/
├─ pingpong-proto      PURE. no I/O, no GPU. wire format, FEC, reassembly, pacer.
├─ pingpong-transport  Tunn + UDP. handshake, rekey, roaming, endpoint tracking.
├─ pingpong-capture    trait FrameSource -> GPU texture. Windows impl (v1).
├─ pingpong-encode     trait VideoEncoder. NVENC/CUDA impl.
├─ pingpong-decode     trait VideoDecoder. VideoToolbox impl.
├─ pingpong-server     bin (Windows). capture -> encode -> proto -> transport.
└─ pingpong-client     bin (macOS).   transport -> proto -> decode -> present.
```

`pingpong-proto` **must not** depend on `pingpong-transport`, any GPU crate, or
any platform crate. If it ever needs to, the boundary is wrong.

The trait seams in `capture`, `encode`, and `decode` exist so v2's Linux host
and v3's alternate codecs are additions rather than rewrites.

---

## 4. Transport

### 4.1 Integration

`Tunn` is used directly as a datagram transform:

```rust
// send
let n = tunn.lock().encapsulate(&inner_packet, &mut out_buf);
socket.send_to(n, peer_endpoint);

// receive
let r = tunn.lock().decapsulate(Some(src_addr), &datagram, &mut out_buf);
```

**[V]** `Tunn::encapsulate` (`noise/mod.rs:618`) performs **no validation** of
`src`. The sender side is entirely unconstrained — any byte string may be
encapsulated.

**[V]** `Tunn::decapsulate` → `validate_decapsulated_packet`
(`noise/mod.rs:1118`) constrains the **receive** side only, and checks exactly:

| check | detail |
|---|---|
| `packet[0] >> 4 == 4` | high nibble only; IHL is never read |
| `packet.len() >= 20` | `IPV4_MIN_HEADER_SIZE` |
| bytes 2–3 (BE u16) ≤ `packet.len()` | returned slice is `packet[..computed_len]` |
| bytes 12–15 | returned to the caller as an `IpAddr`; not otherwise inspected |

Nothing else in the 20 bytes is read. The IPv4 header checksum is **never
verified**. This is what makes D2 possible.

### 4.2 Coupling risk and its mitigation

D2 depends on the *absence* of validation — the class of thing that changes
silently. The fork is pinned at upstream 0.7.0 deliberately, so drift risk is
low, but it must be pinned by a test:

> `pingpong-transport` MUST contain an integration test that constructs a
> pingpong header, round-trips it through a real `Tunn` pair, and asserts byte
> equality. If the fork's validation ever tightens, that test fails loudly
> instead of the stream silently corrupting.

Fallback if it ever does bite: add a raw-payload data message type to the fork.
Avoided for now because it diverges the data hot path of a thesis artifact that
is periodically rebased onto upstream.

### 4.3 MTU ownership

**[V]** `pq_path_mtu` governs **handshake segmentation only**. It does not
affect the data path, and `encapsulate` enforces no size limit. Path-MTU
discovery is an explicit non-goal of the segmentation design
(pq-boringtun's handshake segmentation design).

Therefore **pingpong owns data-path MTU entirely**. Requirements:

1. Set `pq_path_mtu` to the same value used for data-path sizing, so handshake
   and data cannot drift apart.
2. Set **DF (don't fragment)** on the UDP socket, so an oversized path fails
   loudly rather than silently fragmenting and destroying tail latency.
3. Default 1280. Treat 1500 as opt-in after measuring the real path.

---

## 5. Wire format

Every inner packet is exactly 20 bytes of header followed by payload. The header
occupies the IPv4 header's byte positions and satisfies its three validated
constraints; all remaining bytes carry pingpong data.

```
byte  0     0x45            version=4 (REQUIRED). IHL=5 is cosmetic, never read.
byte  1     flags           bit0 keyframe · bit1 frame_end · bits2-3 kind · 4-7 rsvd
bytes 2-3   total_len  BE   REQUIRED: true total length, <= datagram length
bytes 4-5   fragment_idx    u16 LE
byte  6     data_shards     u8
byte  7     parity_shards   u8
byte  8     fec_block_idx   u8
bytes 9-11  frame_len       u24 LE  — true payload length of the whole frame
bytes 12-15 capture_ts_us   u32 LE  — also surfaces as a bogus "src IP"; ignored
bytes 16-19 frame_id        u32 LE
```

`kind`: `0 = video`, `1 = audio` (v2), `2 = input` (v2), `3 = control`.
One tunnel carries every stream; no second socket is ever needed.

### 5.1 Field notes

- **`total_len` must be exact.** The receiver gets back `packet[..total_len]`
  **[V]**. Understating it truncates the payload silently.
- **`fec_block_idx`** exists because Reed-Solomon over GF(2⁸) caps at **255
  total shards**. At 1180-byte payloads that is a ~300 KB ceiling per block. A
  1080p keyframe fits in one block; a 4K keyframe does not. Without this byte,
  4K support would require a wire-format break.
- **`frame_len` is required, not optional.** Reed-Solomon demands identical
  shard sizes **[V]**, so the final data shard is zero-padded before encoding.
  If that final shard is the one lost and recovered, the receiver gets 1180
  bytes back with no way to know how many are real. `frame_len` in every packet
  removes the ambiguity. u24 caps at 16 MB — ample against a ~400 KB 4K
  keyframe.
- **`capture_ts_us` wraps every ~71.6 minutes** (u32 microseconds). All
  arithmetic on it MUST use wrapping subtraction. Since it is only ever used for
  deltas over intervals far below 71 minutes, wrapping is correct and no wider
  field is needed.
- **`frame_id`** as u32 wraps after ~345 days at 144fps. Comparisons must still
  be wrap-aware.
- Bytes 12–15 are additionally returned to us as an `IpAddr` **[V]**. Ignore
  that return value and read the field from the packet slice.

### 5.2 Encoding discipline

Header encode/decode lives in exactly **one** `encode()`/`decode()` pair in
`pingpong-proto`, with the table above reproduced as a doc comment. The layout
is deliberately unusual; it must not be open-coded anywhere else. Round-trip
property tests are mandatory.

---

## 6. Bandwidth and MTU budget

```
1280  path MTU (IPv6 minimum; matches tested segmentation)
 -40  IPv6 header            (IPv4 path: -20, giving 1200 payload)
  -8  UDP
 -32  WireGuard data         [V] 4 type + 4 idx + 8 counter + 16 tag (session.rs:196-229)
 -20  pingpong header        (= the IPv4 header, NOT additional)
────
1180  media payload per datagram
```

**[V]** There is no 16-byte padding: `session.rs:211` notes the spec requires it
but the implementation omits it and works.

### 6.1 Profiles

Payload 1180 B. Core % is against the **measured** 490 Mbps single-core figure
from `pq-boringtun/BENCHMARKS.md` Table 3 **[V]**.

| Profile | Bitrate | Packets/s | Avg data shards/frame | Tunnel core % |
|---|---:|---:|---:|---:|
| 1080p60  | 20 Mbps | 2 119 | 36  | 4.1 % |
| 1080p120 | 35 Mbps | 3 708 | 31  | 7.1 % |
| 1080p144 | 40 Mbps | 4 237 | 30  | 8.2 % |
| 1440p120 | 50 Mbps | 5 297 | 45  | 10.2 % |
| 4K60     | 60 Mbps | 6 356 | 106 | 12.2 % |

That 490 Mbps is the **pessimistic** configuration: encrypt *and* decrypt, four
syscalls, and a TUN device, all on one core. Our path does one crypto pass and
one syscall with no TUN, so real cost is below these figures. Exact per-packet
cost can be pinned with `pq-boringtun`'s existing `crypto_benches`.

**Conclusion: the post-quantum tunnel is not a latency or throughput concern in
steady state.** Its entire cost is concentrated at handshake and rekey (§11.4).

### 6.2 FEC parameters

```
parity_shards = max(2, ceil(data_shards * 0.20))
```

The floor is load-bearing. A static-scene P-frame can be a single packet, where
a pure 20% ratio rounds to zero parity — leaving the frame unprotected precisely
when protection is cheapest.

Split into multiple FEC blocks when `data_shards > 200`, incrementing
`fec_block_idx`. Each block is encoded and recovered independently.

### 6.3 `reed-solomon-simd` constraints

**[V]** from the crate's `Error` enum — all three are load-bearing:

1. **Every shard must be the same size** (`DifferentShardSize`). Zero-pad the
   final data shard before encoding. Transmit it at its *true* length to avoid
   wasting bandwidth; the receiver re-pads before decoding, using `frame_len`
   (§5.1) to recover the real length.
2. **Shard size must be non-zero and even** (`InvalidShardSize`). 1180 is even.
   Any MTU-derived payload size MUST be rounded **down** to even before use.
3. **Not all `(original, recovery)` combinations are supported**
   (`UnsupportedShardCount`) — this crate is Leopard-RS based. Call
   `ReedSolomonEncoder::supports(data, parity)` and, if false, increment
   `parity` until it returns true. Never assume the §6.2 formula yields a
   supported pair.

Use the reusable `ReedSolomonEncoder`/`ReedSolomonDecoder` with `reset()` per
frame rather than the one-shot `reed_solomon_simd::encode` free function —
at 60–144 fps the one-shot API reallocates on every frame.

---

## 7. Framerate and pacing

### 7.1 The binding constraint

**[V] — measured, risk #4 resolved.** WGC delivers one frame per *present of the
composited desktop*. Capture rate is therefore bounded by the **host display's
active refresh mode**, not by the game's internal framerate. A game rendering
144fps to a 60Hz display yields 60 captured frames per second.

Measured on the dev host (RTX 3070 Ti, 1920×1080 @ 60 Hz) by
`pingpong-capture/examples/rate.rs`, which drives a window around the screen for
the active phase:

| desktop | delivered |
|---|---|
| moving window | **59.99–60.00 fps** — exactly the refresh mode |
| genuinely idle | **2.00 fps** |

The active rate pins to the refresh mode and never exceeds it, so >60 fps does
require a ≥120 Hz display mode (§15.3).

Consequences:

> **Streaming above 60fps requires the host display — physical or virtual — to
> be in a ≥120Hz mode.** On a headless host this makes the virtual display a
> *prerequisite for high framerate*, not a convenience. See §15.3.

A physical high-refresh monitor satisfies this equally well: what matters is the
active display *mode*, not whether the display is virtual. The VDD earns its
place by (a) working headless, (b) exceeding whatever monitor is attached,
(c) decoupling the streamed resolution and refresh from the physical one, and
(d) leaving the physical desktop untouched while streaming.

The effective ceiling is:

```
min(host display Hz, client display Hz, encoder throughput, link capacity)
```

Client-side: MacBook ProMotion panels cap at 120Hz; non-ProMotion at 60Hz. **120
is the realistic target**; 144 only with an external high-refresh display.

### 7.2 Design

Capture rate and encode rate are **decoupled**. `pingpong-proto` owns a pacer:

- `target_fps` is configuration, valid 30–240.
- The capture thread pushes every frame WGC delivers.
- The pacer selects which frames to encode against `target_fps`, dropping the
  rest at the earliest possible point — **before** the CUDA kernel, never after.
- When `target_fps` ≥ display refresh, every frame is encoded and the pacer is a
  pass-through.

Dropping early matters: a frame discarded after encode has consumed GPU time and
NVENC capacity for nothing.

### 7.2.1 Delivery is present-driven, not clock-driven

**[V] — confirmed, and a trap worth stating explicitly.** WGC delivers on
*present*, so the display's refresh rate is a **ceiling, not a tick**. When the
desktop is static nothing presents, and delivery drops to near zero: **2.00 fps
measured on a still desktop**, which is a blinking console caret rather than WGC
itself. Implications:

- The pacer MUST tolerate irregular and sparse arrival. Do **not** build it
  around a fixed-rate loop or assume a steady 16.7 ms cadence — it will stall
  the moment the screen goes still.
- ~~Sparse delivery on a static screen is desirable: unchanged content should not
  be re-encoded. No keepalive frames are needed; the client simply continues
  displaying the last decoded frame.~~ **[V] WRONG — corrected 2026-08-04, see
  §7.2.2.** True of the *picture* and false of *control*: it leaves the host with
  no clock, and `RequestKeyframe` is the one thing a client can ask the host to
  do.
- The client MUST NOT treat a gap in frames as a stall or a lost connection.
  Liveness is the tunnel's concern (WireGuard keepalives via `update_timers`),
  never the video stream's.

### 7.2.2 The host needs a clock of its own

**[V] — the correction to §7.2.1's "no keepalive frames are needed".**

The host encodes only inside the capture callback, so with delivery
present-driven, *everything* downstream of capture runs on the host desktop's
schedule rather than on the session's. That is fine for pixels — an unchanged
screen genuinely has nothing to send — and wrong for anything a client asks for:

- **`Control::RequestKeyframe` cannot be answered.** The host sets a flag; the
  flag is consumed in the capture callback. On a still desktop no callback runs,
  so a client that joins mid-session, or loses a frame FEC cannot repair, asks
  twice a second (`STALL`) and stays black until something unrelated happens to
  redraw the host's desktop. The IDR count over a run equals the number of
  *presents* during it, not the number of requests.
- **"The client continues displaying the last decoded frame" assumes it has
  one.** A client joining during an idle period has never decoded anything.
- §7.2.1's "the client MUST NOT treat a gap as a stall" was written against a
  client that had no stall detector. It has one, and it is right to: a gap it
  cannot distinguish from a lost keyframe is exactly when it should ask.

So the host re-encodes the most recent captured frame when capture goes quiet
(`pingpong-server`'s `repeat`):

| condition | deadline |
|---|---|
| a keyframe request is pending | **2 frame intervals** since the last encode (33 ms at 60 fps) |
| otherwise | **100 ms** since the last encode |

Two intervals rather than one for the keyframe case because one is exactly the
spacing of a live stream, so a repeat armed at one interval fires in the gap
between two on-time captures and injects a whole extra IDR into a stream that
needed nothing. 100 ms for the heartbeat because it is well under the client's
500 ms `STALL`: the point is not only to answer requests quickly but to leave
the client no reason to make one.

**Measured on the test host 2026-08-04** by `pingpong-encode`'s `idle_repeat`
example: 2.50 fps captured on an idle desktop, and a keyframe request answered
in **21.2 ms and 2.8 ms across two runs, by a repeat, with no capture
involved**. The cost is ~10 P-frames per second of unchanged content at a
**measured 2121 B mean — ~170 kbit/s, under 1% of a 20 Mbit/s stream**. Under
CBR that had to be measured rather than assumed; rate control is free to spend
the budget it is given.

This also fixes the "frozen stream" in §5.2's note on absolute input: a captured
display that never changes now still produces frames.
- `windows-capture`'s `minimum_update_interval_settings` can *cap* delivery
  rate; it cannot raise it. It is not a substitute for the pacer, but may be
  useful as a cheap pre-filter when `target_fps` is well below refresh.

> **[V] "Idle" is a property of the host, not of WGC.** The first measurement on
> the dev host read **59.99 fps with nothing being done to the machine** — because
> Wallpaper Engine was animating the desktop background at the refresh rate. With
> it paused, the same build read 2.00 fps. Anything that animates (live wallpaper,
> a playing video, a visualiser) keeps the desktop presenting continuously.
>
> Nothing in the design breaks under this — the pacer already handles the full
> rate. But two claims above must be read as *conditional on the host actually
> being still*: "delivery drops to near zero" and "unchanged content should not be
> re-encoded". On a host with a live wallpaper the encoder runs flat out whether or
> not the user is doing anything, and the idle-power argument for sparse delivery
> does not apply.



### 7.3 Encoder pipelining

**[V] The default is 1, not 2 — the measurement inverted this.** The original
argument was: at 144fps the per-frame budget is 6.9 ms while encode *latency* is
3–6 ms, so serialising `submit → block → submit` cannot keep up and only
pipelining makes it feasible.

Encode was then measured at **2.37 ms p50 / 2.45 ms p95** (`spikes/nvenc-min`),
and conversion at 0.46 ms, so the serial cost is **~2.9 ms** — comfortably inside
the budget at every rate v1 targets (16.7 ms at 60fps, 6.9 at 144, 4.2 at 240).
The premise for pipelining does not hold.

It is not merely unnecessary, it is harmful here. `VideoEncoder::encode` returns
at most one frame per call, so with 2 outstanding, frame N's bitstream is not
returned until the caller submits frame N+1 — **a full frame period later**. At
60fps that is 16.7 ms added to glass-to-glass to hide a 2.4 ms encode. Serial
submission returns frame N in 2.4 ms.

So: a bounded number of frames in flight, **default 1**, configurable. The
machinery handles any depth, and raising it is still right if encode ever exceeds
the frame interval — on a slower GPU, at a higher resolution, or with a heavier
preset. Measure before changing.

### 7.4 v1 testing

v1 is validated at 1080p60. The pacer, `target_fps` config, and pipelining are
implemented in v1 so higher rates require no structural change — but rates above
60 are not a v1 success criterion, because they depend on the virtual display
(§15.3).

---

## 8. GPU pipeline (host)

### 8.1 Why a `CUarray` is required

GPU textures are stored **swizzled** — texels ordered along a space-filling
curve so 2D-adjacent texels share cache lines, which is what makes bilinear
filtering fast. The exact pattern is vendor-specific, undocumented, and varies
by GPU generation, format, and dimensions. Modern NVIDIA parts may additionally
apply delta colour compression, so the bytes are not merely reordered but
compressed.

`cuImportExternalMemory` conveys **size, not layout**. Its two exits:

- `cuExternalMemoryGetMappedBuffer` → `CUdeviceptr`: flat bytes. Correct **only**
  for genuinely linear allocations.
- `...GetMappedMipmappedArray` → `CUarray`: an image. Reads go through the
  texture units, which deswizzle **in hardware**.

> Deswizzling is a hardware texture-unit function. It cannot be done in software
> and is reachable only through a `CUarray`. Mapping a tiled texture as a buffer
> yields bytes in an uninterpretable order.

Failure mode is silent: every call succeeds, the encoder produces a valid H.264
stream, and the image is a blocky mosaic. **Do not debug this at the encoder.**

### 8.2 The path

**[P]** `cudarc`'s safe layer exposes only `ExternalMemory::from_handle` and
`get_mapped_buffer`; it has no `CUarray` exit and no D3D11 graphics interop.
Additionally, external memory requires a shared NT handle from
`IDXGIResource1::CreateSharedHandle`, which needs
`D3D11_RESOURCE_MISC_SHARED_NTHANDLE` — a flag `Direct3D11CaptureFramePool` does
not set.

> **[V] CORRECTION — measured, `spikes/cuda-interop/README.md`.** This section
> previously concluded that external memory "would therefore require an extra
> per-frame GPU copy", implying graphics interop would not. **That is false.**
> `cuGraphicsD3D11RegisterResource` on a WGC pool texture returns
> `CUDA_ERROR_INVALID_HANDLE` — the texture cannot be registered at all. A
> per-frame `CopyResource` into a texture we create with
> `D3D11_RESOURCE_MISC_SHARED` is **mandatory** on this route too, and
> registration then succeeds.
>
> Graphics interop remains the right choice (D6) because it reaches a `CUarray`,
> which external memory cannot — but not because it avoids a copy. Neither route
> does. Measured cost of the whole per-frame sequence including the copy: 0.419 ms
> p50 at 1024×768.

Use the graphics-interop API, which takes `ID3D11Resource*` directly:

```
WGC ID3D11Texture2D (BGRA, tiled)
  │ cuGraphicsD3D11RegisterResource(&res, tex, NONE)     ← ONCE per pool texture
  ▼
CUgraphicsResource
  │ cuGraphicsMapResources(1, &res, stream)              ← per frame
  │ cuGraphicsSubResourceGetMappedArray(&arr, res, 0, 0)
  ▼
CUarray ──cuTexObjectCreate──▶ CUtexObject
  │ kernel: tex2D<float4>(tex,x,y) → BGRA→NV12 (+ scale)
  ▼
pitched CudaSlice<u8> ──▶ NVENC register_generic_resource(CUDADEVICEPTR)
```

**[P]** These symbols live in `cudaD3D11.h`, not `cuda.h`, so they are likely
absent from `cudarc`'s generated `sys` bindings. Declare them manually:

```rust
#[link(name = "nvcuda")]
extern "C" {
    fn cuGraphicsD3D11RegisterResource(
        out: *mut CUgraphicsResource, d3d_resource: *mut c_void, flags: c_uint) -> CUresult;
    fn cuGraphicsMapResources(
        count: c_uint, res: *mut CUgraphicsResource, stream: CUstream) -> CUresult;
    fn cuGraphicsSubResourceGetMappedArray(
        out: *mut CUarray, res: CUgraphicsResource, array_idx: c_uint, mip: c_uint) -> CUresult;
    fn cuGraphicsUnmapResources(
        count: c_uint, res: *mut CUgraphicsResource, stream: CUstream) -> CUresult;
}
```

`cudarc` still provides context, streams, module loading, `CudaSlice` for the
NV12 output, and kernel launches. We add four symbols, not a parallel stack.

### 8.3 Requirements

1. ~~**Cache registrations.** `cuGraphicsD3D11RegisterResource` per frame would
   cost more than it saves. WGC recycles a small texture pool — key a map on the
   texture pointer and register each exactly once.~~
   **[V] OBSOLETE — see the §8.2 correction.** WGC textures cannot be registered
   at all, so there is nothing to key a map on. Instead: create ONE
   interop-flagged staging texture, register it ONCE for the session, and
   `CopyResource` each arriving frame into it. Simpler than the pointer-keyed
   registry this requirement described, and it removes the need for a
   `TextureRegistry` type entirely.
2. **Precompile PTX at build time** and embed with `include_bytes!`. Calling
   `compile_ptx` at runtime would require NVRTC and the CUDA toolkit on the host.
   **[V] Doubly required:** runtime NVRTC does not merely add a dependency, it
   does not work here — `cudarc::nvrtc::compile_ptx` on cudarc 0.16.6 + CUDA 12.6
   aborts with `Expected symbol in library: GetProcAddress ... code 127`
   (`spikes/cuda-interop`). Note `nvcc` also needs MSVC `cl.exe` located for it
   (`-ccbin`) when building outside a Developer Command Prompt.
3. **Scaling belongs in the conversion kernel.** A client requesting 1080p from a
   4K desktop costs nothing extra — the kernel already samples through a texture
   object.
4. NVENC config: **P1 preset, `NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`, no
   B-frames, zero reorder delay, infinite GOP with on-demand IDR.** **[V]** the
   P1 + ULL preset path is documented in `nvidia-video-codec-sdk` 0.4.0.
5. **[V] Picture-type decision must be OFF, and the client must then drive the
   GOP completely.** "Infinite GOP with on-demand IDR" needs both halves, and the
   two requirements conflict in this crate: with PTD *on*, NVENC ignores
   `pictureType` and an IDR can only be forced via `NV_ENC_PIC_FLAG_FORCEIDR` in
   `encodePicFlags`, which `nvidia-video-codec-sdk` 0.4.0's safe API does not
   expose at all (`EncodePictureParams` has no flags field). Measured:
   `request_keyframe` silently did nothing — 300 frames, one IDR, the requested
   one absent.

   Turning PTD off makes `pictureType` authoritative, but then **two further
   fields become the client's job**, each of which produced 300 IDRs in 300
   frames until set (confirmed with `ffprobe`, not just NVENC's reported type):
   - `encodeCodecConfig.h264Config.idrPeriod` — H.264 carries its own, and
     setting `NV_ENC_CONFIG::gopLength` alone does not touch it.
   - `NV_ENC_PIC_PARAMS_H264::refPicFlag = 1` on every submitted frame. The safe
     wrapper otherwise sends a zeroed `codecPicParams`, so no frame is marked as
     a reference, every P frame has nothing to predict from, and the encoder
     falls back to intra.

   With all three set, `pingpong-encode`'s example measures exactly 2 I-frames in
   300 (frame 0 and the one requested at 150), 298 P, zero B.

### 8.4 Earliest possible verification

Register one WGC texture, run the conversion kernel, write the NV12 output
directly to a PNG — **before any encoder, packetiser, or socket exists**. Tiling
errors appear instantly, at the one moment when nothing else could cause them.
This is the first task of implementation.

---

## 9. Client pipeline

VideoToolbox decode → `CVPixelBuffer` → `CVMetalTextureCache` → Metal texture →
present. No copy.

**[V]** `kVTDecompressionPropertyKey_RealTime` is **accepted** (`noErr`) and the
decoder does not buffer. Task 1 measured, paced at 60 fps on a 600-frame
1080p no-B-frame stream: **0.96 ms p50 / 1.12 ms p95 / 2.67 ms p99**, in-flight
depth never above **1** — one callback per submit, zero reordering, zero decode
errors. That is ~7× under the 8 ms threshold, so §12's 3–8 ms decode estimate
was conservative.

Task 14's integration test locks the property in: it waits for each frame's
callback before submitting the next, so a decoder that starts buffering hangs
the test rather than passing quietly.

**[V]** Decoded output is **bi-planar 4:2:0** (two planes: Y and CbCr), asserted
per frame in that same test. This is what makes the `CVMetalTextureCache` path
below a two-texture bind with no format conversion; a single- or three-plane
output would need a conversion pass between decode and present.

`objc2-video-toolbox` and `objc2-metal` provide bindings. **[P]** `wgpu` is
deliberately not used — no clean IOSurface import path, which would force a
readback and squander the zero-copy work on both ends.

**[V]** The whole path works: decoded `CVPixelBuffer` → two `CVMetalTexture`s
(R8 luma, RG8 chroma) → BT.709 fragment shader → `CAMetalLayer`. Verified by
photographing the client's own window while it replayed a known test pattern —
colour primaries, gradient ramp and orientation all correct, which is what a
range or matrix error would have broken. `pingpong-client --play <file.h264>`
reruns that check, and separates "the client is broken" from "the link is
broken" without needing a host.

**[V]** `CAMetalLayer.displaySyncEnabled` is set to **false**. Left on, the
layer waits for vblank inside `nextDrawable` and adds up to a frame period of
latency — the same trade the no-jitter-buffer rule rejects.

**No jitter buffer.** Present each frame as soon as it is complete. Buffering
trades exactly the property this project exists to optimise.

---

## 10. Threading and queue discipline

Host:

```
[capture+encode]  WGC callback → pacer → CopyResource into the interop texture
                  → NV12 kernel → NVENC submit+retrieve
        │  bounded(2), drop-oldest
[packetize+send]  RS encode → header → Tunn::encapsulate → sendto
[timer]           update_timers(dst) every 250 ms → send result if any
[receive]         recvfrom → Tunn::decapsulate → control / feedback
```

> **[V] The host split is `capture+encode` / `packetize+send`, not
> `capture+submit` / `output+send`.** That original split assumed NVENC
> submission and completion were separate steps worth putting on separate
> threads. At the chosen pipeline depth they are not: `VideoEncoder::encode`
> submits and retrieves in one call, because `max_in_flight` is 1 (see §7.3).
> The boundary still does its job — the network never blocks capture — and it
> keeps Reed-Solomon off the capture thread, where it would delay the next
> frame.

Client:

```
[receive+decode]  recvfrom → Tunn::decapsulate → Reassembler
                  → VTDecompressionSessionDecodeFrame → callback
        │  1-slot mailbox, newest wins
[present]         CVMetalTextureCache → NV12 shader → CAMetalLayer  (MAIN THREAD)
[timer]           update_timers(dst) every 250 ms
```

**[V]** Present is on the main thread because AppKit requires it, and the
receive thread wakes the event loop with a user event rather than the loop
polling — a spinning main thread would burn a core to gain nothing.

The mailbox holds **1**, not the 2 below: a display can only show one frame, so
keeping a second only guarantees it is shown late. The decoder's own ready queue
upstream of it does follow the rule.

**[V]** `Tunn::update_timers` (`noise/timers.rs:168`) must be pumped externally.
boringtun's own device loop uses **250 ms** (`device/mod.rs:697`). Without it,
rekey, keepalive, and handshake retries never fire.

**[V]** `encapsulate` takes `&mut self`, so `Tunn` is shared as
`Mutex<Tunn>` (use `parking_lot`) across the send, timer, and receive threads.
Contention is ~2 100 short critical sections/sec at 1080p60 — negligible, and
identical to what boringtun's device layer already does.

### 10.1 Queue rule

> **Every inter-thread channel is bounded at 2 with drop-oldest.**

Latency in streaming almost never comes from slow code; it comes from buffers
filling during a transient stall and never draining. An unbounded channel
between encode and send silently converts a 100 ms hitch into 100 ms of
permanent added latency — and it profiles perfectly clean.

---

## 11. Connection lifecycle

### 11.1 Identity and pairing

Each endpoint holds:
- an X25519 static keypair (WireGuard identity), and
- an ML-KEM-768 static keypair for the fork's static-KEM authentication
  (message types 8/9).

`pingpong keygen` emits both. Public halves are copied into the peer's TOML
config once, by hand. **No pairing protocol, no PIN flow, no mDNS discovery** —
there is one server and one client, and anything more is invented work. See
§15.5.

Host config sets `pq_path_mtu = 1280` and calls `set_pq_static_auth(..)` **[V]**
(`noise/mod.rs:541`). Client stores the host endpoint (DDNS hostname or static
IP).

### 11.2 Reconnect

**[V]** `noise/mod.rs:632-635`: with no live session, `encapsulate` queues the
packet and returns a handshake initiation on its own. The send loop simply
transmits whatever it is handed. **No reconnect state machine is needed.**

Per the `decapsulate` contract **[V]**, on `TunnResult::WriteToNetwork` the call
must be repeated with an empty datagram until `Done`, or queued packets stall.

### 11.3 Roaming

WireGuard rebinds a peer's endpoint on any authenticated packet, so a client
moving from café Wi-Fi to phone tethering follows the tunnel automatically —
something the current Moonlight setup cannot do without reconnecting.

Required implementation: on successful `decapsulate`, update the stored reply
address from the `src_addr` passed in. Without this, replies continue to the
stale endpoint and the stream dies on network change.

### 11.4 Rekey under load — the v1 acceptance test

WireGuard rekeys roughly every 120 s (`REKEY_AFTER_TIME`). The fork's phase-2 PQ
initiation is ≈2 420 B, which at 1280 MTU is **3 segments** **[V]**
(segmentation design §2.4) — versus classical WireGuard's single 148-byte
initiation. Those segments land amid 2 119 packets/sec of video.

> **Test:** 10 minutes sustained at the target profile (≥5 rekeys). Assert no
> frame loss attributable to rekey windows, correlating rekey timestamps against
> frame-completion telemetry.

This is the one risk with no prior art to borrow from — no other project
combines a segmented PQ handshake with a latency-critical media stream. If it
fails, mitigations to consider: raising the rekey interval, pacing segments
between media packets, or prioritising segments in the send queue.

**Result: PASS [V]** (`pingpong-server/tests/rekey_under_load.rs`, 601 s,
2026-07-31). 36 000/36 000 frames completed across 5 rekeys, with 12 segmented
handshake messages — 2 initial plus 2 per rekey, so every rekey exercised the
segmentation path. Completion latency within 500 ms of a rekey (n=267) matched
the rest of the run (n=35 733) at every percentile: p50 0.23 / p95 0.31 /
p99 0.33 ms. The worst frame of the entire run (3.32 ms) fell *outside* a rekey
window; the worst inside one was 0.40 ms. **None of the three mitigations was
needed.** Full figures: [v1-measurements.md](v1-measurements.md).

Scope of that evidence: the test runs both tunnels over loopback, so it proves
rekey survives *load*, not *loss*. On a lossy path the same 3-segment
all-or-nothing initiation looks fragile — see the establish-time observation in
the measurements doc, where every observed handshake delay was a multiple of
`REKEY_TIMEOUT`. That is an open question for v2, not a v1 acceptance failure.

---

## 12. Latency budget and telemetry

Target, 1080p60. Values are **[P]** except where marked **[V]**.

| Stage | Expected | Notes |
|---|---|---|
| Capture (WGC arrival) | 1–3 ms | plus inherent ≤16.7 ms frame wait at 60Hz |
| CUDA BGRA→NV12 | **0.43 ms p50 / 0.49 p95** @1920×1080 | **[V]** `spikes/cuda-interop/README.md`. Whole per-frame sequence incl. the mandatory `CopyResource`, map, texobject, launch, unmap. Budget holds. Cost is near resolution-independent up to 1080p (0.406 ms at 1024×768 vs 0.430 at 1080p) — fixed per-call overhead dominates, not pixel throughput. |
| NVENC P1 ULL | **2.37 ms p50 / 2.45 p95** | **[V]** `spikes/nvenc-min/README.md`. Serialised submit→available; §7.3 pipelining should hide most of it. Zero B-frames confirmed by `ffprobe` (0 B / 1 I / 119 P). |
| Packetise + RS encode | <0.5 ms | `reed-solomon-simd` |
| **Tunnel encapsulation** | **<4% of one core** | **[V]** derived from measured 490 Mbps |
| Network | RTT/2 | measure your own path |
| VideoToolbox decode | **1.1 ms p95** | **[V]** measured, `spikes/vt-latency/README.md`: p50 0.96 / p95 1.12 / p99 2.67 ms at a paced 60 fps. Real-time flag accepted; one callback per submit, no buffering. |
| Metal present | ≤16.7 ms | largest client-side cost; one vsync at 60Hz |

### 12.1 Telemetry is not optional

`capture_ts_us` ships in the header from the first commit. The client records
frame-complete and present timestamps against it. **Without per-stage
attribution, v1 produces a number that cannot be acted on — which defeats its
entire purpose.**

Log per-frame: capture→encode, encode→send, send→receive, receive→decode,
decode→present. Report p50/p95/p99, never means alone; tail latency is what is
actually felt.

---

## 13. Testing

### 13.1 `pingpong-proto` — pure, fast, no hardware

The reason for D9. All of this runs with no sockets and no GPU:

- **Property tests** (`proptest`) over a simulated lossy link — drop, reorder,
  duplicate, delay. Invariant: every frame either reconstructs **byte-exactly**
  or is **cleanly declared lost**. Silent corruption is the failure to hunt.
- Shard permutations: any `k` of `n` recovers; any `k-1` does not.
- Header round-trip, including the wrapping-arithmetic edge cases in §5.1.
- FEC block splitting at the 255-shard boundary.
- Pacer: correct selection at every `target_fps` / capture-rate combination,
  including target > capture rate.
- **Fuzz target on the depacketiser.** It parses hostile network input; this is
  the one component an attacker can reach with arbitrary bytes.

### 13.2 Integration — requires hardware

1. Tunnel round-trip byte-equality through a real `Tunn` pair (§4.2 — the test
   that pins D2).
2. Capture → encode produces a decodable stream (verify with an external
   decoder).
3. Single-machine end-to-end loopback.
4. **Rekey under sustained load** (§11.4).

---

## 14. Risks, ranked

| # | Risk | De-risking action | When |
|---|---|---|---|
| 1 | **Rekey under load** disturbs the stream. No prior art. | §11.4 acceptance test | v1, early |
| 2 | **D3D11→CUDA interop**: symbols absent from `cudarc`; tiling silently wrong | §8.4 single-frame PNG test | **first task** |
| 3 | **VideoToolbox real-time decode** not honoured → blows the latency budget | Standalone decode-latency probe before wiring the pipeline | v1, early |
| 4 | ~~**WGC capture rate bounded by display refresh** (§7.1)~~ **[V] RESOLVED as assumed** | Measured by `pingpong-capture/examples/rate.rs`: 59.99 fps active against a 60 Hz mode, never above it; 2.00 fps idle. Delivery is present-driven, so >60 fps needs a ≥120 Hz display mode (§15.3). | done |
| 5 | ~~**NVENC init path** in the Rust crate less trodden than CUDA examples~~ **[V] RESOLVED** | Measured in `spikes/nvenc-min`: init/config/encode all work at 2.37 ms p50 with zero B-frames. Two non-obvious requirements found — NVENC cannot register cudarc's `cuMemAllocAsync` memory (use `result::malloc_sync`), and `enable_picture_type_decision()` is mandatory. | done |
| 6 | ~~**WGC textures lack the shared flag** (assumed in D6)~~ **[V] RESOLVED, opposite to the assumption** | Measured in `spikes/cuda-interop`: WGC textures cannot be registered with CUDA at all (`CUDA_ERROR_INVALID_HANDLE`), so a `CopyResource` into an interop-flagged texture is mandatory. This **weakens** D6's stated rationale rather than strengthening it — see the §8.2 correction. | done |

Risks 2, 3, and 5 are all cheap standalone spikes. **Run all three before
writing any pipeline code.** Each one, if it fails, changes the design.

---

## 15. v2 and beyond — future work

Explicitly **not** in v1. Recorded so the v1 seams are cut correctly.

### 15.1 Input (v2, highest value)
Client: `winit` (keyboard/mouse), `gilrs` (gamepad). Host: `SendInput` via the
`windows` crate; virtual gamepad via ViGEmBus + `vigem-client`.

**Design note:** input packets carry **no FEC and no retransmission**. Each
carries a small ring of the last ~4 events with sequence numbers, so a dropped
packet self-heals on the next one. Retransmitting an already-stale keypress is
worse than useless. Uses `kind=2`; the wire format already accommodates it.

ViGEmBus is a driver install — the project's second driver dependency, and the
reason input is not in v1.

### 15.2 Audio (v2)
WASAPI loopback capture → Opus (5 or 10 ms frames) → `kind=1` on the same
tunnel. **Enable Opus in-band FEC (LBRR)** rather than routing audio through
Reed-Solomon; it is purpose-built and costs a few kbps against a 20 Mbps video
stream.

A/V sync: both share the `capture_ts_us` timebase. Start with independent
playback — drift below ~40 ms is imperceptible for game streaming — and add
slaving only if a problem is actually audible.

### 15.3 Virtual display (v2 — promoted from v3)

**Promoted because §7.1 makes it a prerequisite for >60fps, not a convenience.**

**Do not write a driver.** IddCx is C++/WDK and needs an EV certificate to
distribute. Reuse an existing signed VDD (Apollo bundles one; `VirtualDisplayDriver`
is MIT-licensed). pingpong's job: install/enable it, set the mode via
`ChangeDisplaySettingsEx` — **including the target refresh rate** — capture that
display, and disable it on disconnect. **[P]** Which VDD is currently
best-maintained must be verified before committing.

### 15.4 Adaptive bitrate (v2/v3)
A loss-and-queuing-delay controller (the Parsec/Moonlight approach), not WebRTC's
GCC. Feedback travels client→server in `kind=3` control packets, which is where
it belongs — it does not need to ride in every video header, and §5's former
reserved bytes are now spent on `frame_len`. v1 uses a fixed, manually configured
bitrate, as Moonlight is used today.

### 15.5 Pairing and discovery (v3)
mDNS on LAN; a short-code or QR pairing flow to replace hand-copied TOML. Only
worth building for more than one client.

### 15.6 Linux host (v3)
Only three traits differ — capture, input injection, display control. **The
encoder does not**, and D5 helps here: NVENC on Linux takes `CUdeviceptr`
identically, and DMA-BUF handles *are* shareable by design, so
`cuImportExternalMemory` works there even though it fails on WGC textures. The
`CUarray` abstraction holds on both platforms; only the import call differs.

Capture via KMS/DRM (dedicated box) or the PipeWire portal (Wayland session).
Input via `uinput` — no signed driver needed, unlike ViGEmBus.

### 15.7 Codecs (v3)
HEVC and AV1 for better quality per bit. H.264 in v1 because it has the lowest
encode latency and universal VideoToolbox decode support. The `VideoEncoder` /
`VideoDecoder` traits exist for this.

### 15.8 Deferred indefinitely
Multi-client, web client, HDR, mobile clients, file transfer, clipboard sync.

---

## 16. Appendix: verified claims

Every **[V]** claim, with its citation. Re-verify if the fork advances past its
pinned 0.7.0 base.

| Claim | Source |
|---|---|
| `Tunn` is a pure userspace state machine; no TUN required | `boringtun/src/lib.rs`, `noise/mod.rs` |
| `encapsulate` performs no validation of `src` | `noise/mod.rs:618-635` |
| Receive validates only version nibble, ≥20 B, `total_len`, and reads src IP | `noise/mod.rs:1118-1160` |
| IPv4 header checksum is never verified | `noise/mod.rs:1118-1160` |
| Data overhead is exactly 32 B; no 16-byte padding | `noise/session.rs:196-229`, note at :211 |
| `encapsulate` with no session returns a handshake initiation | `noise/mod.rs:632-635` |
| `update_timers` must be pumped externally; device loop uses 250 ms | `noise/timers.rs:168`, `device/mod.rs:697` |
| `set_pq_path_mtu`, `set_pq_static_auth` exist and are per-`Tunn` | `noise/mod.rs:518`, `:541` |
| PMTU discovery is an explicit non-goal | segmentation design doc, line 32 |
| Phase-2 PQ init ≈2 420 B → 3 segments at 1280 MTU | segmentation design doc §2.4 |
| Measured 490 Mbps single-core (M4 loopback, full device path) | `pq-boringtun/BENCHMARKS.md` Table 3 |
| NVENC P1 + `ULTRA_LOW_LATENCY` preset path exists in the Rust crate | `nvidia-video-codec-sdk` 0.4.0 docs |
| `register_generic_resource` accepts `CUDADEVICEPTR` resources | `nvidia-video-codec-sdk` 0.4.0 docs |
| `cudarc` exposes `ExternalMemory::from_handle` + `get_mapped_buffer` only | `cudarc` docs, `driver::safe::external_memory` |
