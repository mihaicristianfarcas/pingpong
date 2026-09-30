# pingpong v2 — Design

> **Historical record.** Kept as it was written, for the reasoning and the
> measurements behind the code; code comments cite its sections. Where it
> and the code disagree, the code and the [current docs](../README.md) are
> right. See [the design history](README.md).

**Status:** approved design, not yet implemented
**Date:** 2026-07-31
**Scope:** v2 only. Builds on `2026-07-29-pingpong-v1-design.md`, which is
referenced throughout as "v1 §n". Future work is enumerated in §13.

v2 makes pingpong *usable*: a virtual display the client specifies, and keyboard
and mouse input. v1 proved the pipeline; v2 makes it something you can sit in
front of.

---

## How to read this document

Same two tiers as v1, marked inline:

- **[V]** — verified against source, measurement, or a cited upstream artefact at
  design time. Trust these.
- **[P]** — provisional: reasoned from experience or documentation, **not**
  measured. Treat every `[P]` as a hypothesis to confirm during implementation.

Implementers: when a `[P]` turns out wrong, update this document rather than
working around it locally. v1 §8.2 and §7.3 are the precedent — both were
inverted by measurement, and correcting them in place is why the v1 doc is still
trustworthy.

---

## 1. Scope

### 1.1 What v2 is

```
macOS client                                     Windows host
────────────                                     ────────────
client TOML: mode + bitrate
  └─ SessionStart (kind=3) ═══════════════════▶  VDD activate → set mode
     ◀═══════════════════════ SessionAck (actual mode)
                                                   └─ capture THAT display
     ◀═══════════════════════ video (kind=0, unchanged from v1)
winit keyboard/mouse
  └─ input (kind=2) ══════════════════════════▶  SendInput injection
```

Three things, in this order:

1. **A virtual display** on the host, set to a mode the client requests.
2. **A session lifecycle** — the host stops streaming unconditionally and starts
   streaming *a session*, with a control channel to set it up.
3. **Keyboard and mouse input**, absolute and relative, hotkey-toggled.

### 1.2 Why this slice

v1 §7.1 established, by measurement, that capture rate is bounded by the host
display's active refresh mode. That makes the virtual display **a prerequisite
for streaming above 60 fps**, not a convenience — v1 structurally could not
exceed 60 fps on a 60 Hz desktop no matter how fast the pipeline was. The VDD is
what unlocks the headroom v1 measured but could not use.

Input is what makes the stream a computer rather than a video. It is v1 §15.1's
"highest value" item, and it is the first client→host media direction the
project has ever carried.

The two belong in one version because a matched virtual display makes absolute
mouse input geometrically trivial (§5.2), and because a headless host needs a
display before injecting input into it means anything.

### 1.3 Success criteria

1. The client-specified mode is set on the host, captured, and displayed at that
   mode on the client.
2. **Streaming above 60 fps, end to end.** The payoff v1 §7.1 identified and
   could not reach.
3. Keyboard and mouse usable in a real game in relative mode; the Windows
   desktop navigable in absolute mode. Judged by use, not by a number — see
   §1.4.
4. Zero stuck inputs across the §10 acceptance test, and the host display always
   restored, including after a host crash.

### 1.4 Non-goals for v2

Carried forward from v1 §1.4 and still out: audio, adaptive bitrate, HEVC/AV1,
multi-client, discovery/mDNS, a pairing UI, Linux host support, path-MTU
discovery.

Newly out, by explicit decision:

- **Gamepad input.** ViGEmBus is a driver install and the project's second driver
  dependency. Keyboard and mouse first.
- **v1's unfinished measurements.** The glass-to-glass comparison against
  Moonlight/Apollo (v1 §1.3.3) and the lossy-path handshake experiment
  ([v1-measurements.md](v1-measurements.md), "Time to establish the tunnel") are
  dropped. v1 §1.3.3 therefore has no verdict and will not get one in v2.
- **Changes to `pq-boringtun`.** v2 lives in this repo only. If the segmented
  handshake proves fragile on a lossy path, that is a separate project with the
  v1 evidence already in hand.
- **No click-to-photon or glass-to-glass target.** Follows from dropping the
  measurement work. Criterion 3 is a judgement, deliberately.

---

## 2. Decisions and rationale

| # | Decision | Rationale |
|---|---|---|
| E1 | Reuse a signed third-party VDD; do not write a driver | v1 §15.3's rule, unchanged. IddCx is C++/WDK and needs an EV certificate to distribute. **[V]** `VirtualDrivers/Virtual-Display-Driver` is signed, IddCx-based, and the best-maintained option (§6.1). |
| E2 | Display mode comes from the **client**, sent at connect: its presentation surface by default, its TOML if the TOML names one | Keeps the client the source of truth. **[V] CORRECTED 2026-08-03** — the original decision was "from the client's TOML" on the grounds that it avoided the Retina backing-vs-logical ambiguity. It does not avoid it, it relocates it onto a human: the number that makes geometry 1:1 is the size of the surface the picture lands on, and on macOS that is neither the panel's pixel count nor the display mode's. A 14" MacBook Pro panel is 3024×1964, but this client's scaled mode has a 3600×2338 backing store and presents borderless-fullscreen at **3600×2260**, because the menu bar's strip is reserved. A TOML saying 3024×1964 — the panel's own native size, chosen precisely to get 1:1 — was upscaled and pillarboxed. `stream_width = 0` now means "ask for whatever this client presents at", which is the only value 1:1 by construction, and it is the default. |
| E3 | Both mouse modes, hotkey-toggled | **[V]** What Moonlight/Apollo do — they are separate injection paths, not one mechanism (§5.2). Relative is mandatory for games; absolute is mandatory for a usable desktop. |
| E4 | The wire carries **physical scancodes**, not virtual keys or characters | Positional, so the client's macOS layout need not match the host's Windows layout, and it is what games reading raw input/DirectInput expect for WASD. **[V]** Moonlight instead carries Win32 VK codes normalized to US English; we diverge because winit hands us a positional `KeyCode` directly, so a VK table would be a round trip through a representation neither end wants. See §5.1 for the tradeoff this makes. |
| E5 | Input is **unreliable**, healed by redundancy | v1 §15.1's choice, kept. **[V]** Moonlight instead sends input over ENet *reliable*; we diverge deliberately, because a retransmit stalls every subsequent input by at least one RTT, and the measured LAN RTT is 12.5 ms. §5.4 is the price of this decision. |
| E6 | Mouse mode is **not transmitted at all** | `MouseMoveRel` and `MouseMoveAbs` are already distinct event types (§4.3), so the injection path follows from the event itself. The host needs no mode state, so neither a header bit nor a control message is required. Mode is a purely client-side concept: which events it generates and whether it grabs the cursor. |
| E7 | Input send thread **blocks on a queue**; only motion is batched | **[V]** Exactly what Moonlight does (§5.3). A fixed-tick poller wakes 1000×/s on a battery-powered client to do nothing. |
| E8 | Two new crates: `pingpong-display`, `pingpong-input` | v1 §15.6 already named capture, **input injection**, and **display control** as the three platform-varying traits. Cutting them as traits now is what makes a Linux host an addition rather than a rewrite. |
| E9 | Client-side input capture gets **no** crate | The client already owns the winit event loop; wrapping events it already receives is a seam with nothing behind it. The interesting logic (mapping, sequencing, encoding) is pure and goes in `pingpong-proto`, per v1 D9. |
| E10 | Graceful degradation to the physical display, opt-out | `SessionStart` flags bit 0 = *require VDD*. Unset, the host may fall back and reports the actual mode. This **decouples input development from the VDD spike's outcome** (§12). |

---

## 3. Crate layout

Additions to v1 §3:

```
pingpong-proto     + control::{SessionStart, SessionAck, SessionEnd}   kind=3
                   + input::{InputEvent, InputBatch, scancode table}   kind=2
                   + input::SequenceGate                               host-side dedup
pingpong-display   trait DisplayControl  → Windows VDD impl            (NEW)
pingpong-input     trait InputSink       → SendInput impl              (NEW)
```

`pingpong-proto`'s constraint from v1 §3 is unchanged and now covers the new
modules: **no dependency on `pingpong-transport`, any GPU crate, or any platform
crate.** The scancode table, the sequence gate, and every codec in §4 are pure
and testable with no winit, no Windows, and no sockets.

### 3.1 Refactors this version requires

Not optional cleanups — v2 cannot be built cleanly without them.

- **`pingpong-server/src/pipeline.rs` (438 lines) splits** into `session.rs`
  (lifecycle, control handling) and `stages.rs` (thread bodies). **[V]**
  `pipeline::run()` currently starts capture immediately and streams
  unconditionally; §7 makes the stages start/stoppable, and one file doing both
  lifecycle and pipeline work is the file that will grow badly.
- **`pingpong-client/src/main.rs` (832 lines) splits**, gaining `input.rs`
  (winit → `InputEvent`, grab management) and `session.rs` (SessionStart/Ack).
  It is the largest file in the project *before* input capture is added to it.

No other refactoring. v1 §3's trait seams in capture/encode/decode are untouched.

---

## 4. Wire format

Both new kinds keep v1 §5's 20-byte header **unchanged**, per v1 §5.2's rule
that header encode/decode lives in exactly one place. Unused FEC fields
(`data_shards`, `parity_shards`, `fec_block_idx`, `frame_len`) are zeroed. v1
§5's invariants still bind: byte 0 must be `0x45`, and `total_len` must be exact
**[V]**, or the receiver gets a silently truncated payload.

### 4.1 Fields reinterpreted by kind

Two header fields carry different meanings per `kind`. This table is an
extension of v1 §5's, and belongs beside it in the doc comment:

| Field | `kind=0` video | `kind=2` input | `kind=3` control |
|---|---|---|---|
| `capture_ts_us` | capture time | client event time | send time |
| `frame_id` | frame number | sequence number of the **newest** event in the packet | message id |

Wrap-aware comparison is mandatory on both, per v1 §5.1.

### 4.2 No new header flags

v1 §5 byte 1's reserved bits 4–7 stay reserved. An earlier draft of this design
spent bit 4 on a `mouse_mode` flag; it was removed on review as redundant, since
§4.3 already distinguishes `MouseMoveRel` from `MouseMoveAbs` by event type and
the host derives the injection path from that. See E6.

### 4.3 Input payload (`kind=2`)

```
byte 0      event_count (1..=8)
bytes 1..   event_count records, tagged:

  0  KeyDown        u16 scancode   (set 1; E0 extended encoded in the high bit)
  1  KeyUp          u16 scancode
  2  MouseMoveRel   i16 dx, i16 dy
  3  MouseMoveAbs   u16 x,  u16 y  (stream pixels; see §5.2)
  4  ButtonDown     u8 button      (0=L 1=R 2=M 3=X1 4=X2)
  5  ButtonUp       u8 button
  6  Wheel          i16 dv, i16 dh
  7  HeldState      u8 n, then n×u16 scancode, u8 m, then m×u8 button
                    — reserved; not implemented in v2. See §5.4.
  8  Text           u32 Unicode scalar (v3: the clipboard typed on the host)
```

**[V] `MouseMoveRel` is `i16`, but winit's deltas are `f64`.**
`DeviceEvent::MouseMotion` carries `(f64, f64)` "in unspecified units", so the
client must round — and rounding by truncation loses slow motion entirely, since
a 0.4-pixel-per-event drag truncates to 0 forever. The batcher therefore
**carries the fractional residue** across events: accumulate in `f64`, send
`trunc()`, keep the remainder for the next batch. See §5.3.

Events in a packet are **contiguous in sequence**, spanning
`frame_id - event_count + 1 ..= frame_id`. No per-event sequence number is
transmitted; the receiver derives them. This is what makes the redundancy in
§5.4 free.

**The header's `fragment_idx` carries a client GENERATION for `kind=2`**, not a
fragment index — input is never fragmented, so the field was otherwise zero.

Added 2026-08-04, after two reports of a session that streamed perfectly and
accepted no input. The sequence watermark in §5.4's gate is meaningless across a
client restart: the new process counts from 0, so every packet it sends is
"older" than the watermark its predecessor left, and the host discards all of it
until the new client has sent as many events as the old one did. Nothing in the
host notices, because a repeated `SessionStart` for the mode already running is
answered by §7's re-ack, which deliberately changes no state.

The generation cannot be inferred: a restart lands at sequence 0, which is only
a few hundred behind a short-lived predecessor's watermark and so is
indistinguishable from ordinary reordering. Resetting the gate on the re-ack
instead would double-inject — `SessionStart` retransmits every 100 ms until
acked, so several land after activation (eight in the 2026-08-04 14:47 run), and
each reset would re-admit the whole ring and re-press every held key.

So the client stamps every packet with a per-process nonce and the host rebases
its watermark when that value changes. A host that sees the field always zero —
an older client — behaves exactly as before.

### 4.4 Control payload (`kind=3`)

```
SessionStart  c→h   u8 op=2, u16 width, u16 height, u32 refresh_mhz,
                    u32 bitrate_bps, u8 flags                          14 B body
SessionAck    h→c   u8 op=3, u8 status,
                    u16 width, u16 height, u32 refresh_mhz             10 B body
                    ↑ ACTUAL mode, not the requested one
SessionEnd    both  u8 op=4, u8 reason                                  2 B body
CursorState   h→c   u8 op=5, u8 flags, u8 shape, u16 x, u16 y           7 B body
                    flags bit0 visible · bit1 clipped
```

**`CursorState` is what makes the mouse mode automatic** (added 2026-08-05;
supersedes the hotkey in §5.2). The host reports what its FOREGROUND
APPLICATION is doing with the pointer, and the client follows: hidden or
clipped-to-a-point means lock and send relative, anything else means release
and send absolute with that shape drawn.

**[V] Nothing here is inferred.** `AttachThreadInput` to the foreground
window's thread, then `GetCursor`, returns NULL exactly while a game has taken
the mouse — 75 of 180 samples across four clean CS2 gameplay/menu transitions,
measured 2026-08-05. `clipped` is a second, independent signal (CS2 confines
the pointer to 1×1 at the screen centre throughout gameplay); both agreed on
every sample, and either alone is sufficient, since neither behaviour is
contractual. An earlier draft was going to infer the mode by comparing sent
deltas against the reported position — a heuristic where this is a fact.

`x`/`y` are in stream pixels. The client draws its OWN cursor at its OWN
position, which is what makes the pointer lag-free where streaming the host's
bitmap would cost an RTT; the coordinates are only for resync when the host
moves the pointer itself.

Sent on change, plus a 500 ms heartbeat: control carries no FEC and no
retransmit, so one lost datagram would otherwise strand the client in the
wrong mode indefinitely.

**[V] Opcode numbering is constrained by existing code, not free.**
`pingpong-proto/src/control.rs` already defines `OP_REQUEST_KEYFRAME = 1` for
v1's sole control message, so session messages take **2, 3, 4**. Opcode `0`
stays unused, so an all-zero body is not a valid message. An earlier draft of
this section numbered from `msg=0` and would have collided with v1's keyframe
request on the wire.

**[V] `Control::encode` must change shape.** It currently returns a fixed
`[u8; CONTROL_LEN]` where `CONTROL_LEN = HEADER_LEN + 1`, which cannot express a
body carrying fields. It becomes
`encode(self, capture_ts_us: u32, out: &mut [u8; MAX_CONTROL_LEN]) -> usize`,
where `MAX_CONTROL_LEN = HEADER_LEN + 14` is sized by `SessionStart`. This is a
breaking change to the two existing call sites (server and client keyframe
requests), which must be updated in the same commit.

- `refresh_mhz` is **millihertz**, not Hz. Fractional refresh rates are real
  (59.94), and **[V]** the chosen VDD advertises floating-point refresh support.
  `u32` millihertz covers 240000 with room to spare.
- `flags` bit 0 = *require VDD* (E10).
- `status`: `0=ok · 1=mode unsupported · 2=VDD unavailable · 3=busy`.
- **`SessionAck` returns the mode the host actually set**, which may differ from
  the request. The client sizes its Metal layer to what it is told, never to what
  it asked for. Getting this backwards produces a stream that is silently
  letterboxed or cropped.
- **`refresh_mhz` in the ACK is the rate frames will arrive at, not the display
  mode the host set.** The two are normally equal — capture is present-driven, so
  a 120 Hz display delivers 120 fps — but they come apart when the host's
  configured `target_fps` caps below its own display's refresh rate. The ack must
  report the capped rate. **[V] 2026-08-04:** with the ack reporting the display
  mode instead, a host set to `target_fps = 60` presented a 120 Hz virtual
  display, acked 120, and delivered 60, with `dropped_pacer = 0` throughout
  because the pacer was discarding half the frames exactly as configured. Nothing
  in the telemetry contradicted the ack, and it read as a virtual-display fault.
  The width and height fields keep their meaning — they are the mode, and they
  are what the client sizes to.
- **The WINDOW is only sized to the acked mode when it is not fullscreen.**
  **[V] 2026-08-03** — a fullscreen window is already exactly the surface it will
  present on, and asking for another size does not enlarge the screen: on macOS
  it shrinks the window *inside* the fullscreen space, so the picture covers part
  of the display and the black behind it shows through on the other two sides.
  Measured: a 3600×2260 fullscreen window asked to become 3024×1964 presented at
  3024×1964 with the remainder black, and AppKit restored the fullscreen size
  ~2.4 s later — which is why the fault looked intermittent and why leaving and
  re-entering fullscreen appeared to improve it. Fullscreen scales to fit and
  letterboxes instead; with E2's deferred mode there is nothing to scale.

Control has no FEC. `SessionStart` retransmits every 100 ms until acked, bounded
at **30 s**, then fails loudly. Host handling is **idempotent**: setting a mode
already set is a no-op, so a duplicated `SessionStart` costs nothing.

**[V] The bound was ~5 s and that was shorter than the host.** The ack is not
sent until `activate` finishes, and a mode the virtual display has not held
before takes longer than that to bring up: §6's `ARRIVAL_TIMEOUT` alone is 10 s
of waiting for the monitor to attach, and the *failing* path waits that out and
then up to 5 s more for it to depart before acking `VddUnavailable`. Measured
2026-08-03: the client logged "host never acked SessionStart after 5s; giving
up" while the host was still working, and the ack landed after it had gone. The
bound is now derived from that worst case rather than chosen, so a give-up means
the host is genuinely not answering.

---

## 5. Input

### 5.1 Keys

**[V]** winit's `PhysicalKey`/`KeyCode` is positional, and `SendInput` with
`KEYEVENTF_SCANCODE` injects positionally. Mapping `KeyCode` → PS/2 set-1
scancode is a pure table in `pingpong-proto` (E4).

**The tradeoff E4 makes, stated explicitly.** **[V]** With `KEYEVENTF_SCANCODE`
set, Windows derives the virtual key from the scancode using the **host's**
active keyboard layout. For positional input — WASD, hotkeys, modifiers — that
is exactly right and is why games work. For *text entry* it means the character
produced follows the host's layout, not the client's: a US-layout client typing
on a host set to German gets German characters. This project streams games, so
positional correctness wins; the alternative (Moonlight's US-normalized VK
codes) optimises the opposite case. Record it here so nobody debugs it as a bug.

**[P]** Keys with no US-layout equivalent have no defined scancode in the table.
Moonlight needed a protocol flag (`SS_KBE_FLAG_NON_NORMALIZED`) for exactly this
case, which is evidence it occurs in practice. v2's table maps what winit's
`KeyCode` enumerates and **drops anything unmapped rather than guessing** —
guessing produces a wrong keypress, which is worse than none. If dropped keys
turn out to matter, the fix is a `NonNormalized` event type, not a wire break.

**[P]** macOS intercepts system-reserved combinations (Cmd-Tab, Cmd-Q, Cmd-Space)
before winit sees them. Capturing those requires Accessibility / Input Monitoring
permission and probably a `CGEventTap` rather than winit alone. **v2 degrades
gracefully:** everything else works without the permission, and the shortcuts
that don't forward are documented rather than worked around.

### 5.2 Mouse

**[V]** Moonlight and Apollo implement absolute and relative as *separate
injection paths*, not one unified mechanism: Apollo injects `MOUSEEVENTF_MOVE`
for game mode and `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` for
remote-desktop mode, and Sunshine's `input.cpp` runs a scale-and-offset transform
only on the absolute path.

**Relative is mandatory for games and this is not a resolution problem.** An FPS
hides the cursor, locks it, re-centres it every frame, and reads raw motion —
there is no cursor position to set, at any resolution, on any display, virtual or
physical. **[V]** Apollo issue #1479 is this exact failure: injected absolute
events reach raw-input games (GTA V, FiveM) flagged `MOUSE_MOVE_ABSOLUTE`, the
game computes a delta against a cursor it is itself re-centring, and the camera
snaps to a corner.

**What the VDD actually buys, stated honestly.** It does *not* delete the
absolute-mode coordinate transform. If the client renders the stream in a window
rather than fullscreen, window coordinates still need converting to stream
pixels. What it buys is that the transform becomes **trivial and client-side**:
the client converts window → stream coordinates before sending, and the host
applies them raw. The host stays ignorant of client geometry, which is the part
that makes Moonlight's version bug-prone. Fullscreen at the negotiated mode is
the degenerate 1:1 case.

**Cursor rendering.** The client hides its own cursor in **both** modes and lets
the streamed host cursor be the only one visible — v1 already captures
`CursorCaptureSettings::WithCursor` **[V]**
(`pingpong-capture/src/windows.rs:81`), so it is already in the frame. Two
cursors on screen is worse than one lagging cursor. The honest cost: in absolute
mode the visible cursor lags by one network RTT. Moonlight solves this with local
cursor rendering; v2 accepts the lag and §13 records it.

**Where relative deltas come from.** **[V]** `DeviceEvent::MouseMotion`, which
winit documents as "raw, unfiltered physical motion. Not to be confused with
`WindowEvent::CursorMoved`." `CursorMoved` is the pointer-accelerated, screen-
clamped position of a GUI cursor: it stops changing at the edge of the display
and it has the OS's acceleration curve already baked in. Feeding either to a game
gives the player a camera that sticks at the screen edge and an acceleration
curve applied twice. Absolute mode uses `CursorMoved`, which is exactly what that
event is for.

**Cursor grab.** **[V]** winit's `CursorGrabMode::Confined` is *not supported on
macOS* (returns `ExternalError::NotSupported`); `Locked` is the mode that works.
Relative mode uses `CursorGrabMode::Locked` + `set_cursor_visible(false)`;
absolute mode does not grab, because the pointer must be able to leave the
window.

**The cursor is the CLIENT's, always. [V] REVERSED 2026-08-03.** The original
rule was the opposite — hide the client's pointer in both modes and let the
streamed host cursor be the only one, accepting one RTT of lag. It is reversed
because the host's cursor turned out not to be a dependable thing to stream:

- With **no mouse attached** — the normal state of a headless host — Windows
  suppresses the pointer entirely. `SM_MOUSEPRESENT = 0`, `GetCursorInfo`
  reports no `CURSOR_SHOWING` and a null `hCursor`, and nothing is drawn.
  Hit-testing still works, so buttons highlight under an invisible cursor.
  Nothing downstream can recover it: Desktop Duplication — where Sunshine and
  Apollo read the cursor from, out of band, which is how Moonlight gets one —
  reported `Visible=false` on all 483 frames of an 8 s sample and never
  delivered a shape. Moonlight would show no cursor on that host either.
- With a mouse attached, it appears, and lags by an RTT.

So the picture would depend on what happens to be plugged into the host. The
host therefore captures with `CursorCaptureSettings::WithoutCursor` and never
streams a pointer, and the client draws its own in **absolute mode**, with no
configuration either side. One cursor, no lag, same behaviour either way.

Relative mode still hides it: a locked pointer does not move, so drawing it
would pin an arrow to the middle of the screen while the camera turns. A game in
relative mode draws its own reticle into the frame, which streams as picture.

**The session opens in absolute mode.** The first thing a connecting user sees is
a desktop they need to click on; grabbing the cursor before they can launch
anything is a trap that can only be escaped by the hotkey they have not learned
yet.

**Hotkeys**, intercepted client-side and **never forwarded**:
`Ctrl+Alt+Shift+M` toggles mouse mode, `Ctrl+Alt+Shift+Q` ends the session.

### 5.3 Send discipline

**[V]** Moonlight's `inputSendThreadProc()` blocks on a queue
(`LbqWaitForQueueElement`); it does not poll a timer. Batching is applied to
motion only, at `MOUSE_BATCHING_INTERVAL_MS 1`, accumulating deltas
(`deltaX += deltaX`). v2 does the same:

> The input thread **blocks on the event queue**. On wake: if the event is
> relative motion, accumulate for a ≤1 ms window, summing deltas, then send. Key
> and button events send **immediately** — batching a keypress adds latency for
> nothing.

Zero wakeups when idle, a ~1000 pps ceiling under a 1000 Hz mouse, and ≤1 ms
added. Against video's ~2119 pps at 1080p60 (v1 §6.1) this is a small increment
on a tunnel measured at <5% of one core.

**[V]** Moonlight splits packets when accumulated deltas exceed `INT16` limits.
§4.3's `i16 dx, dy` inherits that requirement exactly: **the batcher must split,
not saturate**, or fast flicks silently clamp.

### 5.4 Healing: why unreliable input needs help, and how much

The receiver keeps `last_applied_seq` and applies only events above it
(wrap-aware). Duplicates and reorders are therefore **idempotent by
construction** — including relative deltas, which are never double-applied
despite being retransmitted. Each packet carrying the last 8 events means an
isolated loss self-heals from the next packet.

**The gap.** That redundancy is *forward-looking*. It only helps if more packets
follow — and it vanishes exactly when the user stops generating events, which is
precisely when a release happens:

```
seq 41    KeyDown W      t=0
seq 42-47 mouse moves    t=0..999
seq 48    KeyUp W        t=1000     packet carries seq 41..48 — and is LOST
                                    user stops touching anything → no next packet
host: last_applied_seq = 47. W held down forever.
```

**The fix: exponential-backoff trailing repeat.** When the queue drains, re-send
the same packet eight more times, waiting **1, 2, 4, 8, 16, 32, 64, then 128 ms**
between consecutive re-sends — the delays are inter-send gaps, so the last copy
leaves 255 ms after the original. Re-sends need no host-side logic at all — the
sequence gate already discards them. Any new event cancels the schedule, since
the fresh packet carries the same ring.

A re-send is **byte-identical** to the original, `capture_ts_us` included. That
field is the client event time (§4.1), and the event did happen when it happened;
restamping it would report the retransmit's age as the input's age and quietly
corrupt any latency figure built on it.

The backoff is the load-bearing part, not the repetition. Eight copies packed
into 8 ms all fit inside one Wi-Fi loss burst; the same eight spread across
255 ms do not. **[V]** The client leg is Wi-Fi with 4.8 ms of measured RTT
jitter ([v1-measurements.md](v1-measurements.md)), and Wi-Fi loses in bursts of tens of
milliseconds rather than in isolated packets.

This also covers the mirror case — press and *hold* W with the `KeyDown` lost:
the queue drains, the repeats fire, the host gets the press.

**Escalation, deliberately not built.** It fails only if loss exceeds ~255 ms.
`HeldState` (§4.3 type 7) is reserved for that case: republish the full held set
every 500 ms indefinitely, host reconciles by set difference. It is ~20 lines on
each side. **Build it only if §10's acceptance test produces stuck inputs.**

**[V]** For the record, Moonlight has no such problem because it sends input over
ENet *reliable* (`ENET_PACKET_FLAG_RELIABLE`) — and its own source carries a TODO
wishing for unreliable-sequenced delivery "when we have a delayed reliable
retransmission thread". Their authors treat the unreliable path as *requiring*
retransmission machinery. E5 accepts that trade knowingly; §5.4 is the machinery,
and it is cheaper than acks and ordering.

---

## 6. Display control

### 6.1 The driver

**[V] The driver is SudoVDA** — SudoMaker Virtual Display Adapter 1.10.9.289,
IddCx-based, already installed on the test host by **Apollo 0.4.6**, with
`ApolloService` running. Verified by `spikes/vdd-mode` (2026-07-31). This
resolves v1 §15.3's open **[P]** about which VDD to use.

> **Superseded.** This section previously named
> `VirtualDrivers/Virtual-Display-Driver`, chosen from documentation before the
> host was inspected. Nothing was installed: the spike found a working VDD
> already present, and installing a second IddCx driver alongside a live Apollo
> setup was judged the greater risk. The choice was confirmed with the user.

**[V] The mode is a creation parameter — but it is not sufficient.**
`IOCTL_ADD_VIRTUAL_DISPLAY` takes width/height/refresh and a *previously unseen*
monitor comes up at exactly that mode. On every subsequent session Windows
reapplies a configuration it has persisted against the monitor's EDID identity,
which overrides it: a measured ADD for 1920×1080 @ 60 produced 2560×1440 @ 120.

So `activate()` **must force the mode after the monitor attaches** and verify by
read-back. `ChangeDisplaySettingsEx` does this correctly on the virtual display
(return 0, matching read-back) — issue #471's symptom does not reproduce; only
`CDS_SET_PRIMARY` is rejected (§6.3).

**[V] Arrival and departure are asynchronous.** `REMOVE` returns before the
display detaches. The host identifies its display by the `adapterLuid` +
`targetId` that ADD returns, resolved through
`QueryDisplayConfig`/`DisplayConfigGetDeviceInfo`, never by diffing GDI names
against a snapshot — during a mode change the outgoing display is still listed,
so a name diff never observes the new one.

**[V] `RefreshRate` accepts millihertz** — the driver does
`if (VSync < 1000) VSync *= 1000`, so §4.4's `refresh_mhz` passes through
unconverted.

**[V] ADD is idempotent by `MonitorGuid`**, returning the existing monitor's
`adapterLuid`/`targetId` without recreating it — which is exactly the
idempotency `DisplayControl::activate` requires (§4.4). It returns the existing
monitor *even if the requested mode differs*, so a mode **change** must be
REMOVE-then-ADD.

**[V] A keepalive is mandatory.** SudoVDA runs a global 3-second watchdog; every
IOCTL except `IOCTL_GET_WATCHDOG` resets it, and at zero the driver disconnects
all monitors. Measured: with `ApolloService` stopped, the display was reaped
~2 s after the last ping. The host must ping at ≤1 s from its own thread —
depending on Apollo's polling would be a silent coupling that fails when the
user stops it.

### 6.2 The trait

```rust
pub trait DisplayControl {
    fn activate(&mut self, mode: DisplayMode) -> Result<ActiveDisplay>;
    fn restore(&mut self) -> Result<()>;
}
```

`ActiveDisplay` identifies *which* monitor to capture. This matters: with a VDD
present the host has two displays, and v1's capture must be pointed at the right
one rather than at whatever it defaults to.

**[V] The seam for this already exists.** `WgcSource::new` takes a monitor index
(`pingpong-server/src/pipeline.rs:103` calls it as `WgcSource::new(0)`), so
`ActiveDisplay::monitor_index` feeds it directly and no change to
`pingpong-capture` is required. What changes is only *when* the source is
constructed: v1 builds it once at startup, v2 builds it per session, after the
mode is set.

### 6.3 The risk this whole section rested on — resolved

Settled by `spikes/vdd-mode` (2026-07-31, the test host, build 26200). See
`spikes/vdd-mode/README.md`.

**[V] Issue #471 does not apply.** It is a defect in
`VirtualDrivers/Virtual-Display-Driver`, which §6.1 no longer selects. SudoVDA
sets the mode at monitor creation, so the failing call — a post-hoc
`ChangeDisplaySettingsEx` mode change — never happens. Measured: a request for
2560×1440 @ 120 Hz produced exactly that, next to the physical 144 Hz panel.

**[V] The host is on build 26200** (Windows 11 25H2). Irrelevant now, but
recorded because the finding was measured there.

**[V] The CCD API is required — for setting primary only.** A plain
`ChangeDisplaySettingsEx` mode change on the virtual display works (§6.1). This
converts the previous **[P]**, and in the direction it predicted, though the
reason differs from the one hypothesised:

- `ChangeDisplaySettingsEx(CDS_SET_PRIMARY)` returns `DISP_CHANGE_FAILED (-1)`
  on the indirect display, with the correct multi-display `CDS_NORESET`
  staging sequence. The same sequence returns `0` for the physical display.
- `QueryDisplayConfig` → translate every source position by the target's offset
  → `SetDisplayConfig(SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG |
  SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE)` **succeeds**, and the virtual
  display becomes primary.

**[V] The blocking scenario does not occur.** §6.3 previously feared that the
virtual display could not be made primary, which would mean games launch on the
physical display and are never captured. It can be made primary. Phase 1's
premise holds.

**[V] The virtual display has an unambiguous identity.**
`IOCTL_ADD_VIRTUAL_DISPLAY` returns `adapterLuid` + `targetId`, which match
`DISPLAYCONFIG_PATH_INFO::targetInfo` directly. No "last enumerated display"
heuristic is needed anywhere in the implementation.

**[V] Display work must run in the interactive desktop session.** Session 0 —
where SSH lands — reports a synthetic `WinDisc 1024×768` and never sees the real
monitors. This was known for WGC (`spikes/run-interactive.ps1`); it applies to
display enumeration and mode-setting equally.

### 6.4 Restore after a crash

If `pingpong-server` dies with the VDD active, nothing restores the desktop. On
`activate()` the host **writes the pre-session display state to disk**; on
startup, a stale state file means "restore this, then continue." Cheap, and it
stops a crash from leaving the user's physical desktop in the wrong mode.

With SudoVDA the "wrong mode" is narrower than §6.4 originally assumed — the
physical display's own mode is never altered, so what must be restored is the
**primary assignment and desktop positions** the CCD call changed (§6.3), not a
resolution.

**[V] Two failure modes, not one:**

1. **The process dies.** The watchdog reaps the virtual display within ~3 s on
   its own, so the display itself self-heals. The state file exists to restore
   *primary* and the desktop arrangement.
2. **The process lives but stops pinging** (wedged thread, long stall). The
   watchdog reaps the display out from under a running session. The capture
   source then points at a display that no longer exists, which §9 must treat as
   a session-fatal error rather than a transient capture failure.

### 6.5 A live session must inhibit display sleep

**[V]** Added 2026-08-01, after phase 1 measured it
([v2-measurements.md](v2-measurements.md)). Frames stopped after ~14 minutes and never
resumed while the session and tunnel stayed perfectly healthy — `established=true`,
no teardown, no error anywhere. Cause: `powercfg /query SCHEME_CURRENT SUB_VIDEO
VIDEOIDLE` returns `0x384` = 900 s, so Windows powered the display off at its
15-minute idle timeout, the desktop stopped compositing, and change-driven
capture correctly had nothing to send.

This is not a capture defect. It is the host having no reason to stay awake:
locally a user is at the keyboard, and remotely nobody is. **It is currently the
only known way a healthy session dies.**

So a session asserts `SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED
| ES_SYSTEM_REQUIRED)` for its duration and clears it with `ES_CONTINUOUS` on
teardown.

**[V] The flags are per-thread**, so this must live on a thread whose lifetime is
exactly the session's — asserting it on a thread that then exits silently
un-inhibits. §6.1's mandatory keepalive thread already has precisely that
lifetime (started by `activate`, stopped by `restore`), so it is the correct
home and no new thread is required.

Injected input (§5) also resets the idle timer as a side effect, which is why
this was not caught earlier and why it must still be done explicitly: it would
otherwise fail only on an *idle* session, which is the hardest case to attribute.

---

## 7. Session lifecycle

v1's host streams unconditionally. v2's host streams *a session*:

```
idle
 └─ SessionStart received
     ├─ DisplayControl::activate(mode)     VDD up, mode set, made primary
     ├─ start capture of THAT display      frames GATED, see below
     ├─ start encode / packetize / send stages
     ├─ InputSink::open()
     ├─ send SessionAck(actual mode)
     └─ open the gate                      frames start flowing here

 └─ SessionEnd received, or tunnel dead
     ├─ InputSink::release_all()           backstop for §5.4
     ├─ stop stages
     └─ DisplayControl::restore()
```

**The gate is why the ack comes last and the stream still does not.** The ack
has to report the mode that was *actually* set, and a mode is only proven by the
stages starting against it — so the stages cannot wait for the ack. But an
ungated start puts frame 0 on the wire before the client knows the session
exists, and worse, queues the ack behind that frame's 200–365 datagrams. The
socket answers `WouldBlock` under exactly that load and `Tunnel::send_datagram`
abandons a datagram after 50 ms of it, so with no FEC and no host-side
retransmit on control, "the ack is delayed behind frame 0" and "the ack is lost
behind frame 0" are the same event. A lost ack costs the client a 100 ms
retransmit cycle at best, and its whole 30 s `GIVE_UP_AFTER` at worst.

So frame production is held on a flag the session loop opens immediately after
the ack is sent. The cost is the handful of captures discarded in that window,
which on an idle desktop is none.

**[V]** `handle_inner()` (`pingpong-server/src/pipeline.rs:351`) is the existing
hook where inbound inner packets land; control and input dispatch from there.

**Session liveness is the tunnel's business, never input's.** Input silence is
normal — the user is not always touching anything. v1 §7.2.1 established exactly
this rule for video ("the client MUST NOT treat a gap in frames as a stall"), and
it applies identically here. Session death is detected from tunnel state, from
which v1 already derives `established`.

### 7.1 Tunnel state is not, by itself, session death

**[V]** `established` goes false during an ordinary rekey. Both
`ConnectionExpired` paths in `pq-boringtun`'s `noise/timers.rs` call
`handshake.set_expired()` and `clear_all()`, so the session really is destroyed
and rebuilt — the signal is accurate, not a glitch. Measured 2026-07-31: an
outage of ~5s, 125s into a session, which healed unaided.

Therefore the host MUST NOT treat the falling edge of `established` as session
death. It declares the session over only after the tunnel has been down
**continuously for `TUNNEL_GRACE` (15s)**, and at most once per outage. 15s sits
above `REKEY_TIMEOUT` (5s) and several of its retries, and far below
`REKEY_ATTEMPT_TIME` (90s), past which boringtun has given up for good.

**[V]** `rx_bytes` from `Tunn::stats()` MUST NOT be used as a liveness signal.
It is incremented only in the data path after the inner IP header is parsed
(`noise/mod.rs:1155`); a keepalive is a zero-length data packet and returns
earlier (`0 => return TunnResult::Done`). A live but quiet peer therefore has a
frozen `rx_bytes`. The signal that *would* be correct, `TimeLastPacketReceived`,
is not exposed by `stats()`.

### 7.2 A host with no session MUST say so

The client cannot infer a lost session from silence. Capture is change-driven
(§6.4), so a host whose screen is static legitimately sends nothing for minutes;
"no frames" and "no session" are indistinguishable at the client. Any timeout
built on frame arrival is therefore wrong by construction.

So the host answers instead: a control message that presupposes a session —
`RequestKeyframe` — received while idle MUST be answered with
`SessionEnd(NoSession)`. The client, on receiving it, re-arms its negotiator and
starts a fresh `SessionStart` retransmit cycle, including a fresh give-up
deadline.

This costs traffic only in the failure case, because a starving client is
already sending keyframe requests. Regression that motivated it (2026-07-31): a
host teardown left the client acked and silent, asking for keyframes twice a
second for six minutes while the stream sat frozen on one frame.

---

## 8. Bandwidth

v1 §6.1's profile table stops at 4K60. v2's target modes exceed it: a
client-native 120 Hz panel at ~5.9 Mpixels is roughly **6× the pixel rate of the
1080p60 v1 was tuned against**.

Consequences to measure, not assume:

- **Encode time.** v1 measured 2.37 ms p50 at 1080p **[V]**; at 6× the pixel rate
  the serial-submission argument of v1 §7.3 (which set `max_in_flight = 1`) must
  be re-checked. If encode exceeds the frame interval, v1 §7.3's machinery
  already supports raising the depth — it was built for exactly this.
- **FEC shard counts.** v1 §6.2's 255-shard cap and `fec_block_idx` block
  splitting were designed for 4K keyframes and **never exercised at 1080p60**.
  They will be now.
- **Bitrate.** Fixed and configured, as v1, but it now ships in `SessionStart`
  (§4.4) because 20 Mbps is wrong for these modes. Adaptive bitrate remains out
  of scope (v1 §15.4).

Input adds ~1000 pps of small packets against video's ~2119 (§5.3) — immaterial
against a tunnel measured at <5% of one core.

---

## 9. Error handling

| Condition | Response |
|---|---|
| `SessionStart` lost | Client retransmits every 100 ms, bounded 30 s, then fails loudly (§4.4) |
| Requested mode unsupported | `SessionAck` status 1 + the mode actually set; client renders to that |
| VDD unavailable | Status 2. If *require VDD* is set, session refused. If not, fall back to the current display and report it (E10) |
| Input packet lost | Sequence gate + ring redundancy + backoff trailing repeat (§5.4) |
| Loss burst > 255 ms | Residual; escalation is `HeldState` (§5.4), not built in v2 |
| Client dies holding a key | `release_all()` on tunnel death (§7) |
| Host crashes with VDD active | Stale state file restores primary + arrangement on next start; the watchdog reaps the display itself (§6.4) |
| Watchdog reaps the display mid-session | Session-fatal, not a transient capture error: tear down and return to idle (§6.4) |
| Host display sleeps mid-session | Prevented, not handled: the session inhibits display sleep for its duration (§6.5). Untreated it looks like a healthy session that silently stops sending frames |
| Duplicate / reordered input | Idempotent by construction — sequence gate discards ≤ `last_applied_seq` |

---

## 10. Testing

### 10.1 Pure, in `pingpong-proto` — no sockets, no GPU, no Windows

Per v1 D9. This is where the interesting logic lives, and all of it is testable
without hardware:

- Scancode table: **exhaustive match**, not totality. Every `KeyCode` winit can
  emit is either mapped to a defined set-1 scancode or named in an explicit
  unmapped list — no catch-all arm. §5.1 drops unmapped keys deliberately, so a
  test demanding a scancode for *every* variant would contradict it; what must
  be guaranteed is that no variant is unclassified. Stated this way, a winit
  upgrade that adds a `KeyCode` fails to compile instead of silently falling
  into a default.
- Input batch and control message encode/decode round-trip, including sequence
  wrap (v1 §5.1).
- Motion batching splits rather than saturates at `i16` bounds (§5.3).
- **The invariant property test.** Under arbitrary drop / reorder / duplication
  of input packets, the sequence of events the receiver *applies* equals the
  lossless sequence. This is v2's analogue of v1 §13.1's "reconstructs
  byte-exactly or is cleanly declared lost."
- Trailing-repeat schedule: a single surviving copy at any point in the backoff
  sequence releases a held key.
- **Fuzz target** on the input and control parsers, extending v1's depacketiser
  fuzz. These parse hostile network input too.

### 10.2 Integration — requires hardware

1. `DisplayControl::activate` / `restore` round-trip; state survives a simulated
   crash (§6.4).
2. WGC captures the **virtual** display specifically, not the physical one.
3. Injection verified by a small Windows probe that reads raw input and reports
   what it saw. "SendInput returned success" is not evidence a game saw
   anything — v1 §8.3.5 learned this when `request_keyframe` silently did
   nothing for 300 frames while reporting success.
4. End-to-end session at the client's native mode, above 60 fps (criterion 2).

### 10.3 The v2 acceptance test

In the spirit of v1 §11.4, one test that is unique to this version's risk:

> **20 disconnect/reconnect cycles, each holding at least one key across the
> disconnect, over an input path with induced burst loss, leave zero stuck
> inputs and the host display exactly as found.**

Burst loss, not uniform loss: uniform per-packet drop is the case §5.4 already
handles trivially, so testing it proves nothing. Drop in bursts of 20–200 ms,
straddling the ~255 ms trailing-repeat window on both sides.

This is the test that decides whether §5.4's escalation gets built.

---

## 11. Threading

Additions to v1 §10. v1's queue rule — **every inter-thread channel is bounded at
2 with drop-oldest** — applies to video only and **must not** be applied to
input.

```
Host, added:
[input apply]   handle_inner(kind=2) → SequenceGate → InputSink::inject
[control]       handle_inner(kind=3) → session state machine

Client, added:
[input send]    winit events → queue → BLOCK on queue → batch motion ≤1ms → send
```

Input is **not** drop-oldest. Dropping the oldest input event is dropping a
keystroke; §5.4's whole design assumes events are delivered in sequence, not
sampled. Input applies on the receive thread directly — injection is a cheap
syscall and does not merit a hop.

**Control is not drop-oldest either, and is bounded at 8.** v1 §10.1's rule
exists to stop *media* buffers absorbing a stall and never draining; a control
channel carries a handful of messages per session, and dropping a
`SessionStart` is not the same kind of event as dropping a frame. Eight is
ample for the retransmit burst §4.4 permits.

---

## 12. Phasing

| Phase | Content | Gate |
|---|---|---|
| **0** | **VDD spike — DONE** (`spikes/vdd-mode`, 2026-07-31). No driver installed: SudoVDA was already present via Apollo. Mode set at creation **[V]**; primary via `SetDisplayConfig` **[V]**; watchdog keepalive mandatory **[V]**. WGC/NVENC capture of the virtual display is deferred to phase 1's end-to-end run (§12 phase 1). | Answered §6.3 before any pipeline code existed |
| **1** | Control channel, session lifecycle, display control, §3.1 refactors | Host sets the client's mode and streams it above 60 fps |
| **2** | Input: relative → absolute → the toggle. Plus display-sleep inhibition (§6.5), which is host session behaviour and has no other phase to belong to. | Usable in a game |

Phase 0 is first for the same reason v1 ran three spikes before writing pipeline
code, and that decision paid three times — most sharply when the WGC texture
turned out not to be CUDA-registrable at all, inverting v1 D6's rationale. **If a
spike can invalidate a design, run it before writing the design's code.**

Phase 0 installs a signed third-party driver on the test host. Per the standing rule
for that machine, ask before running it.

E10 means phase 2 is **not blocked** by phase 0's outcome: with *require VDD*
unset, input can be developed against the physical display.

---

## 13. v3 and beyond

Carried forward from v1 §15, minus what v2 consumed:

- **Audio** (v1 §15.2) — WASAPI loopback → Opus with in-band FEC, `kind=1`. The
  slot is already reserved in the wire format.
- **Gamepad** — ViGEmBus + `vigem-client`. Deferred from v2 for its driver
  dependency; §4.3's event tagging extends to it without a wire break.
- **Adaptive bitrate** (v1 §15.4) — `kind=3` control packets, which §4.4 now
  actually exists to carry.
- **Local cursor rendering** — removes the one-RTT cursor lag §5.2 accepts.
- **`HeldState`** (§5.4) — if the acceptance test demands it.
- **Pairing and discovery, Linux host, HEVC/AV1** — v1 §15.5–15.7, unchanged.
- **v1's dropped measurements** — glass-to-glass vs Moonlight, and the
  segmented handshake over a lossy path. Still worth doing; not in v2.

---

## 14. Appendix: verified claims

| Claim | Source |
|---|---|
| Apollo injects `MOUSEEVENTF_MOVE` (game) vs `MOUSEEVENTF_ABSOLUTE\|VIRTUALDESK` (desktop); Sunshine transforms only on the absolute path | Apollo issue #1479; Sunshine `src/input.cpp` |
| Absolute injection breaks raw-input games — camera snaps to corner | Apollo issue #1479 |
| Moonlight sends input over ENet **reliable**; TODO wishes for unreliable-sequenced "when we have a delayed reliable retransmission thread" | `moonlight-common-c/src/InputStream.c` |
| `LiSendKeyboardEvent` carries Win32 VK codes "interpreted as keys on a US English layout"; `LiSendKeyboardEvent2` adds `SS_KBE_FLAG_NON_NORMALIZED` for keys that don't map | `moonlight-common-c/src/Limelight.h` |
| Moonlight documents absolute mouse motion as having limited game compatibility, relative preferable as default | same |
| With `KEYEVENTF_SCANCODE`, the scancode is used and the VK derived from the host layout; otherwise the VK is used | `MapVirtualKey` / `SendInput` docs, Microsoft Learn |
| `inputSendThreadProc()` blocks on `LbqWaitForQueueElement`, no timer poll | same |
| `MOUSE_BATCHING_INTERVAL_MS 1`; relative deltas accumulate | same |
| Packets split when deltas exceed `INT16` | same |
| winit `CursorGrabMode::Confined` unsupported on macOS; `Locked` works | winit docs, `winit::window::CursorGrabMode` (re-confirmed 2026-08-01 against winit 0.30: `Confined` "Not implemented" on macOS; `Locked` lists only X11/iOS/Android) |
| `DeviceEvent::MouseMotion` is "raw, unfiltered physical motion. Not to be confused with `WindowEvent::CursorMoved`"; delta is `(f64, f64)` in unspecified units | winit docs, `winit::event::DeviceEvent` |
| Windows powers the display off at a 900 s idle timeout, ending a healthy session | `powercfg /query SCHEME_CURRENT SUB_VIDEO VIDEOIDLE` = `0x384`, [v2-measurements.md](v2-measurements.md) |
| `VirtualDrivers/Virtual-Display-Driver` is signed, IddCx-based, 8K, floating-point refresh | project README / releases |
| Its last tagged release is 25.7.23 (July 2025) | releases page |
| Issue #471 open, "help wanted": `ChangeDisplaySettingsEx` fails on Win11 24H2/25H2 | VDD issue #471 |
| The test host is Windows 11 build 26200 (25H2) — an affected build | `systeminfo`, 2026-07-31 |
| v1 captures with the cursor drawn into the frame | `pingpong-capture/src/windows.rs:81` |
| v1 starts capture unconditionally; `handle_inner` is the inbound hook | `pingpong-server/src/pipeline.rs:191`, `:351` |
| Client leg is Wi-Fi; LAN RTT 12.5 ms avg, 4.8 ms stddev | [v1-measurements.md](v1-measurements.md) |
| NVENC 2.37 ms p50 at 1080p; `max_in_flight = 1` justified by it | v1 §7.3, `spikes/nvenc-min` |
