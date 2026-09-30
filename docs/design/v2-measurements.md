# v2 phase 0–1 measurements

> **Historical record.** Kept as it was written, for the reasoning and the
> measurements behind the code; code comments cite its sections. Where it
> and the code disagree, the code and the [current docs](../README.md) are
> right. See [the design history](README.md).

What the session lifecycle and the virtual display actually do, measured. See
[v1-measurements.md](v1-measurements.md) for the v1 baseline and for why the camera measurement is the
only one that settles glass-to-glass.

Every figure below is **[V]** — taken from a named run, not estimated.

---

## Hardware and path

| | |
|---|---|
| Host | the test host, Windows, NVENC, SudoVDA virtual display |
| Client | MacBook Pro 14", 3024×1964 native panel, VideoToolbox + Metal |
| Path | **WAN over Tailscale**, direct (not DERP), `203.0.113.5:41641` |
| Negotiated mode | 3024×1964 @ 60000 mHz, borderless fullscreen, 1:1 no scaling |
| Bitrate | 15 Mbps |
| Run | 2026-08-01 06:20–06:42 |

**Path MTU is not free here.** Tailscale defaults both endpoints to MTU 1280,
and that cannot carry this protocol: the PQ handshake segments are up to
`PATH_MTU` (1280) bytes, so the IP packet is 1308 and is dropped. The tunnel
never comes up. Both ends were raised to 1360:

```
sudo ifconfig utun4 mtu 1360                                              # Mac
netsh interface ipv4 set subinterface "Tailscale" mtu=1360 store=active   # host
```

---

## Per-stage telemetry

Mid-run sample, 06:30:00–06:30:02, three consecutive one-second windows.

### Host

| Stage | p50 | p95 | p99 |
|---|---|---|---|
| `capture->encode` | 8.12–8.33 ms | 8.33–8.53 ms | 8.40–8.57 ms |
| `capture->sent` (host total) | 8.40–8.48 ms | 8.56–8.67 ms | 8.57–8.68 ms |

`encode->send` is the difference, ~0.2–0.3 ms: Reed-Solomon, encapsulation and
`sendto` are not where the time goes.

### Client

| Stage | p50 | p95 | p99 |
|---|---|---|---|
| `reassembled->decoded` | 2.54–2.65 ms | 3.09–3.49 ms | 3.99–4.50 ms |
| `recv->presented` (client total) | 5.39–6.77 ms | 17.48–21.84 ms | 34.87–65.31 ms |

The client tail is far worse than the host's and is the honest weak point of
this run. p99 reaching 35–65 ms on a WAN path is jitter arriving from the
network, not decode: `reassembled->decoded` p99 stays under 4.5 ms throughout.
v1's LAN figures do not have this tail.

### Throughput

| | |
|---|---|
| Continuous streaming | 06:21:04 → 06:35:25, **14 min 21 s** |
| Rate | 57–61 fps against a 60 fps target |
| Frames presented | 48,825, `decode_failed=0`, `decode_dropped=0` |
| Host drops | **0** — 849 of 849 seconds at `dropped_pacer=0 dropped_queue=0` |

---

## Session lifecycle under rekey

### Result: PASS with a caveat (2026-08-01, 1283 s run)

Over 21 minutes: **0 teardowns, 0 `ConnectionExpired`, 0 renegotiations**, and
`established` never went false once the session was up. Roughly ten rekeys
happened in that window and none of them disturbed the stream.

The `SessionEnd(NoSession)` mechanism fired once, correctly, at 06:21:03 —
during the 26 s the initial handshake took, the client asked for a keyframe
before the host had a session, and the host answered instead of dropping it.

**Caveat, stated plainly:** the failure this run was built to check did *not*
reproduce. The 2026-07-31 run hit a real `ConnectionExpired` at 125 s that took
the tunnel down for 5 s; nothing like it occurred here. So `TUNNEL_GRACE` is
covered by unit tests (`pingpong-server/src/session.rs`) and by the reasoning in
spec §7.1, but **it has not been exercised in the field**. A run that
deliberately interrupts the path is still owed.

### The regression this replaced

2026-07-31, same configuration: streamed cleanly for 2 min 04 s, then froze
permanently on one frame. `ConnectionExpired` → host tore the session down →
tunnel recovered unaided 5 s later → host idle, client acked and silent,
requesting keyframes twice a second for the remaining six minutes. Both defects
and both fixes are recorded in the plan under Task 10a.

---

## Display sleep ends the stream

Frames stop after ~14 minutes and never resume, while the session and tunnel
stay perfectly healthy (`established=true`, no teardown, no errors).

Cause **[V]**: `powercfg /query SCHEME_CURRENT SUB_VIDEO VIDEOIDLE` returns
`0x00000384` = 900 s. Windows powers the display off at its 15-minute idle
timeout; the desktop stops compositing, and change-driven capture correctly has
nothing to send.

This is not a capture bug — it is the host lacking a reason to stay awake. A
remote host with a live session must inhibit display sleep for the duration of
that session. Not in the phase 0–1 plan; it belongs to whichever phase owns host
power behaviour.

---

## Verdict against the success criteria

**Criterion 1 — the host presents a client-requested mode and captures it:**
**MET.** The client asked for 3024×1964@60, the host created the virtual
display at exactly that mode, acked it, and captured it for 14 minutes with no
drops. The client sized its window to the acked mode and presented 1:1.

**Criterion 2 — streaming above 60 fps:** **MET**, on 2026-08-04 — see the
120 Hz section at the end of this document.

> **The prediction in this paragraph was wrong, and is kept because being wrong
> here cost the first hour of the 120 Hz run.** It read: `capture->encode` p50 is
> already 8.12–8.33 ms at 60 fps and *exceeds* the 8.3 ms interval at 120 fps, so
> v1 §7.3's `max_in_flight = 1` is very likely to need raising, and that is the
> first thing to check.
>
> Neither half held. `max_in_flight` never had to move: at 2560×1440@120
> `capture->encode` p50 is **6.37 ms** **[V]**, inside the budget. And encode was
> never the ceiling in the first place — WGC was delivering only 58 fps, so the
> frames the encoder was supposedly too slow for did not exist. Reasoning forward
> from a stage timing picked the wrong suspect; the thing that settled it was
> counting arrivals with the encoder taken out of the picture entirely
> (`--example rate`). Measure the stage you suspect *in isolation* before
> budgeting against it.

---

# Geometry and the cursor, 2026-08-03

Three reported faults, measured rather than reasoned about. The instrument is
`cargo run -p pingpong-capture --example geometry`, which activates the virtual
display exactly the way a session does, captures one frame, reports the bounding
box of everything that is not black, and writes the frame out as a BMP. Run it
through `spikes/run-interactive.ps1`.

## The black bars were NOT in the video

**[V]** On a 3024×1964 virtual display at **175%** scaling, the non-black
content of the captured frame is the full 3024×1964 — `left=0 top=0 right=0
bottom=0`. Same result on the physical 2560×1440 monitor.

This kills the standing theory. An earlier session blamed DPI virtualisation and
shipped `SetProcessDpiAwarenessContext` in `pingpong-server/src/main.rs` with a
comment claiming an unaware process composites the desktop into the top-left of
a larger capture surface. **It does not.** DWM composites at real pixels
regardless; DPI awareness changes only what *this process is told* when it reads
a coordinate. The call stays — it is what makes `GetSystemMetrics` and
`GetMonitorInfo` report the 175% display as its true 3024×1964, which the
absolute mouse transform is built from — but its rationale was wrong and is now
corrected in place.

## The bars were the client shrinking its own fullscreen window

**[V]** Two faults, both client-side, both visible in one run of the client with
geometry tracing on:

| | |
|---|---|
| Fullscreen window | 1800×1130 points, **3600×2260** physical |
| Requested / acked mode | 3024×1964 |
| What `apply_negotiated_size` did | `request_inner_size(3024×1964)` → window became 1512×982 points |

A fullscreen window asked for a different size does not enlarge the screen; it
shrinks *inside* the fullscreen space. The picture then covered 84% × 87% of the
display and the black behind it showed through on the right and below — the
reported symptom exactly. AppKit restored the fullscreen size 2.4 s later, which
is why it looked intermittent, and why leaving and re-entering fullscreen
appeared to help.

What remained after that self-correction was the second fault: 3600×2260 and
3024×1964 are different aspects (1.5929 vs 1.5397), so the letterbox correctly
pillarboxed at `scale = [0.9666, 1.0]` — ~60 px of black each side — and upscaled
the picture ~19%. **The panel's native size was the wrong number to ask for.**
This Mac runs a scaled mode: 3600×2338 backing store, of which a
borderless-fullscreen window gets 3600×2260 because the menu bar's strip is
reserved. Neither the panel (3024×1964) nor the display mode (3600×2338) is the
surface the picture lands on.

Fixed by deferring the mode to the window (`stream_width = 0`, spec E2) and by
not resizing a fullscreen window (spec §4.4). Verified end to end: negotiated
3600×2260, `letterbox_scale=[1.0, 1.0]`, `render_target=(3600, 2260)`, and a
screenshot of the client showing the desktop edge to edge with no bars.

Also corrected while in there: the layer's `contentsScale` was never set, so it
sat at 1.0 on a 2× display. Setting `layer` before `wantsLayer` makes the view
layer-*hosting*, and AppKit then manages none of the layer's contents
properties. It was right only by accident — the default gravity stretched the
drawable back over the bounds.

## The cursor is not missing from the capture; it is missing from the desktop

**[V]** With the cursor parked mid-screen and moved by `SendInput` (the same
call `pingpong-input` makes), on the **physical** monitor:

```
GetCursorInfo: flags=0x0 (HIDDEN), hCursor=0x0, at (1260, 700)
SM_MOUSEPRESENT: 0
```

Windows believes the test host has no pointing device — its three `HID-compliant
mouse` entries are all `Unknown`/absent — and suppresses the pointer outright.
Hit-testing still works, which is why icons highlight under an invisible cursor.
But nothing is drawn, so nothing can be captured: not WGC, not Desktop
Duplication, not a screenshot. Injecting motion does not lift it; `SendInput`
moves a pointer that is not being rendered.

Not a capture bug and not a virtual-display bug — the same reading comes back on
the physical monitor. `windows-capture` does call
`SetIsCursorCaptureEnabled(true)`, verified in its source, and errors out if the
setting is unsupported.

### `EnableCursorSuppression = 0` does NOT fix it

**[V]** Set to 0 (DWORD) under
`HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e96f-e325-11ce-bfc1-08002be10318}`
and rebooted. After the reboot the flags read `0x2` (`CURSOR_SUPPRESSED`)
briefly and `0x0` once motion was injected — still never `CURSOR_SHOWING`, and
`SM_MOUSEPRESENT` still 0. That key governs the touch/pen suppression feature,
which is a different mechanism from "no pointing device is attached". Reverted.

### And neither would Moonlight + Apollo

The obvious next question is how Sunshine/Apollo — what Moonlight talks to — put
a cursor in the stream, since they clearly manage it. **Architecturally they do
it differently:** their default Windows capture is DXGI Desktop Duplication,
which delivers the pointer *out of band* — `DXGI_OUTDUPL_FRAME_INFO.PointerPosition`
for position and visibility, `GetFramePointerShape` for the bitmap — and they
blend it into the frame themselves before encoding. Our WGC path instead asks
the OS to composite the cursor into the captured image.

That difference does not help here. **[V]** `cargo run -p pingpong-capture
--example cursor`, 8 s with motion injected throughout:

| | |
|---|---|
| Frames acquired | 483 |
| Frames with `PointerPosition.Visible` | **0** |
| Pointer shape updates | **0** |

Desktop Duplication sees no pointer at all, so Sunshine would have nothing to
blend and **Moonlight would show no cursor on this host either**. Moonlight looks
right elsewhere because those hosts have a mouse attached and Windows is drawing
a pointer. The suppression sits below every capture API.

### The fix: the cursor is the client's, always

Attaching a mouse to the host would restore a streamed cursor, but it would make
the picture depend on what is plugged in — a cursor with a mouse, none without,
and an RTT of lag when there is one. So spec §5.2 was **reversed** instead:

- the host captures with `CursorCaptureSettings::WithoutCursor` and never streams
  a pointer, whether or not one exists;
- the client draws its own in absolute mode, unconditionally — no configuration
  on either side.

One cursor, no lag, identical behaviour with or without a mouse on the host.
Relative mode still hides it, because a locked pointer has no position worth
drawing; a game there draws its own reticle into the frame, which streams as
picture. The cost is that the pointer is the macOS arrow rather than whatever
shape the host app would have chosen — an I-beam over a text field still looks
like an arrow.

## The cursor STATE survives, even though the cursor does not, 2026-08-04

The section above concluded that the client must draw its own pointer, because
nothing can capture one. That conclusion was about the *pixels*, and it stands.
It was then read as "the host knows nothing about its cursor", which is false
and is what made the mouse mode a manual toggle.

Suppression stops Windows **painting** a pointer. It does not clear the
per-thread cursor state underneath. `cargo run -p pingpong-capture --example
cursor_state`, 24 samples, mouse physically switched off:

| | |
|---|---|
| `SM_MOUSEPRESENT` | 0 |
| global `GetCursorInfo` | `flags=0x0 hCursor=0x0`, 24/24 — dead, as before |
| **`GetCursor` after `AttachThreadInput`** | **live handle, 24/24** |
| `GetIconInfo` on that handle | succeeds; hotspot 16,16, colour bitmap present |
| `AttachThreadInput` failures | 0 |

**[V]** So the foreground app's cursor — its shape, and the NULL that means the
app has hidden it — is readable on a host with no mouse. That is everything a
client needs to draw the host's real pointer itself, at its own local position,
with **no lag at all**, where a streamed cursor costs a full RTT.

It is also the signal the mouse mode should switch on. A game hiding the pointer
is the app declaring it has taken the mouse, which is a fact rather than the
inference from "the pointer stopped tracking the deltas we sent" that an earlier
draft of this was going to use.

### The game tells us when it has taken the mouse, 2026-08-05

**[V]** The open question above is answered. 90 s, 180 samples, CS2 driven by
hand through gameplay and menus:

| | |
|---|---|
| Samples | 180 |
| `GetCursor` live (ARROW) | 105 |
| `GetCursor` NULL — *the app hid it* | **75** |
| `AttachThreadInput` failures | 0 |

and it switches cleanly, four times, exactly when the game changes state:

```
 0.0s  cmd.EXE   ARROW              clip=full
10.0s  cs2.exe   ARROW              clip=full          <- menu
22.0s  cs2.exe   NULL (app hid it)  clip=1280,720 1x1  <- gameplay
35.6s  cs2.exe   ARROW              clip=full          <- menu
38.6s  cs2.exe   NULL (app hid it)  clip=1280,720 1x1  <- gameplay
41.6s  cs2.exe   ARROW              clip=full
43.1s  cs2.exe   NULL (app hid it)  clip=1280,720 1x1
54.6s  cs2.exe   ARROW              clip=full
```

So the mode switch is a **fact the application declares**, not an inference. No
comparing sent deltas against reported positions, no timing heuristic, no
guessing from whether the pointer moved.

**Two independent signals, and they never disagreed.** Besides the null cursor,
CS2 clips the pointer to a **1×1 rectangle at the screen centre** during
gameplay and releases it to the full desktop in menus. Either alone would carry
the decision; together they cross-check, which matters because a game that hides
the cursor without clipping (or the reverse) is easy to imagine and neither
behaviour is contractual.

**Caveat on this run:** `SM_MOUSEPRESENT` was **1** — the mouse was reconnected,
so the global `GetCursorInfo` tracked the per-thread reading here. The earlier
run is the one that proves the per-thread call survives with no mouse (live
24/24 while the global was dead 24/24). The per-thread call is therefore the one
to build on: it is the only one measured working in *both* states. That CS2's
null-on-gameplay also holds with no mouse attached is not measured, and would be
worth a repeat if the host is ever run headless again.

## Apollo holds the same virtual display driver

**[V]** With `ApolloService` running, `activate` fails: the monitor never
attaches within its 10 s timeout while a 3024×1890@120 SudoVDA display created
by Apollo is present. Stopping the service frees it and activation succeeds.
Both stacks drive the same SudoVDA driver, so they cannot hold a session at
once.

Worth noting alongside it: activation of a **new** mode takes longer than the
client's `GIVE_UP_AFTER` of 5 s (spec §4.4), so a first connection at an
unfamiliar mode can time out client-side while the host is still working. Not
fixed here.

---

# 120 fps end to end, 2026-08-04

**Success criterion 2 is MET.** The link ran at 2560×1440@120 with the host
encoding a sustained `fps=120` and the client presenting 120, no drops on either
side. Getting there took two defects out of the pipeline, neither of which was
the one this document predicted.

Every figure below is **[V]** — from the run named beside it.

## Hardware and path

| | |
|---|---|
| Host | the test host, RTX 3070 Ti, H.264 P1 ultra-low-latency, SudoVDA virtual display |
| Client | MacBook Pro 14", VideoToolbox + Metal, borderless fullscreen |
| Path | **LAN**, direct, `192.168.1.20:51820` (no MTU raise needed) |
| Negotiated mode | 2560×1440 @ 120000 mHz, `require_vdd = true` |
| Requested bitrate | 50 Mbps |
| Run | 2026-08-04 06:04:41–06:09:59, **5 min 18 s** continuous |

**The stimulus is part of the measurement.** Capture is present-driven (§7.2.1),
so a desktop with nothing moving on it cannot demonstrate any frame rate at all —
the first 120 Hz attempt read 58 fps against an idle desktop and looked like a
pipeline fault. The source here is a full-screen `requestAnimationFrame` canvas
in Edge kiosk, repainting every pixel each frame, which reported **rAF 120.0 fps**
on its own on-screen counter (read back by screenshotting the host). Without a
source that genuinely presents at 120, none of the numbers below mean anything.

Wallpaper Engine must also be paused before any *idle* figure is believed — see
the warning already recorded in `pingpong-capture/examples/rate.rs`. It cost a
confusing 61 fps "idle" reading in this run too.

## Per-stage telemetry

Steady state, 06:08:38–06:08:42, five consecutive one-second windows.

### Host

| Stage | p50 | p95 | p99 |
|---|---|---|---|
| `capture->encode` | 6.37–6.39 ms | 6.55–6.69 ms | 6.60–6.83 ms |
| `encode->send` | 0.53–0.54 ms | 1.49–3.00 ms | 1.67–19.52 ms |
| `capture->sent` (host total) | 6.93–6.97 ms | 7.83–9.63 ms | 7.98–25.78 ms |

`capture->encode` p95 of 6.69 ms sits **inside** the 8.33 ms frame interval at
120 fps, which is the direct answer to plan Task 10 Step 3: `DEFAULT_MAX_IN_FLIGHT`
stays at **1**. It was never the constraint.

### Client

| Stage | p50 | p95 | p99 |
|---|---|---|---|
| `recv->reassembled` | 1.30–1.31 ms | 1.73–1.98 ms | 2.08–4.17 ms |
| `reassembled->decoded` | 2.11–2.12 ms | 2.83–2.86 ms | 2.99–3.07 ms |
| `decoded->presented` | 0.25–0.34 ms | 0.33–0.38 ms | 0.35–0.39 ms |

The client tail that dominated the 60 fps WAN run is absent here, as expected of
a LAN path — this is not evidence that it was fixed, only that it was the
network. A WAN run at 120 is still owed.

`decoded->presented` reads 11.4–11.5 ms in the final two seconds of the log.
That is the fullscreen window being closed, not a steady-state figure; it is
0.25–0.34 ms for every one of the preceding ~5 minutes.

### Throughput

| | |
|---|---|
| Host | `fps=120–121`, `repeated=0` |
| Host drops | **0** — `dropped_pacer=0 dropped_queue=0 send_failed=0` throughout |
| Client | `fps=119–121`, **37,560 frames presented** |
| Client drops | `decode_failed=0`, `decode_dropped=1` (one frame, at startup) |
| Wire | 6,441–6,474 datagrams/s ≈ 61.9 Mbps, of which ≈ **50.7 Mbps** is video payload |

---

## Defect 1: WGC's default update interval capped capture at 60 fps

**This was the whole of criterion 2.** The virtual display was at 120 Hz, the
source was presenting at 120, the pacer was dropping nothing (`dropped_pacer=0`
at `target_fps=120`), and the host still encoded 58–60 fps.

The instrument that settled it is `cargo run -p pingpong-capture --example rate`,
which counts WGC arrivals and **does no encoding at all**, run against the live
120 Hz virtual display:

| `MinimumUpdateIntervalSettings` | active | idle |
|---|---|---|
| `Default` (what we shipped) | **58.40 fps** | 2 fps |
| `Custom(4 ms)` | **119.80 fps** | 1.80 fps |

WGC's *default* minimum update interval is ~60 Hz regardless of the display's
refresh mode. The frames were never delivered, so no amount of encoder tuning
could have found them. The idle column is the other half of the result: the fix
raises the ceiling **without** making delivery clock-driven, so a static desktop
still costs nothing and §7.2.1 still holds.

Fixed in `pingpong-capture/src/windows.rs` by setting the interval under the
frame interval of the fastest mode the pacer accepts (`MAX_TARGET_FPS` = 240,
4.17 ms). It is guarded by `is_minimum_update_interval_supported()`, because
`Custom` on a Windows without `GraphicsCaptureSession.MinUpdateInterval` is not a
slower capture — windows-capture rejects the settings and there is no capture at
all.

**Ruled out, with the number that ruled it out:** the frame pool is created with
`numberOfBuffers = 1` (hardcoded in windows-capture 1.5.0), the obvious
suspect for a rate exactly halved. It is not the cause — one buffer sustains
119.80 fps once the interval is lifted.

## Defect 2: NVENC was told 60 fps, so 50 Mbps arrived as ~100

Found only because the wire rate did not match the request. Per-frame datagram
count was **identical** across the two rates — ~104/frame at 60 fps and
~106/frame at 120 fps — so the encoder never re-sized a frame when the frame rate
doubled:

| Run | datagrams/s | per frame | video payload vs 50 Mbps requested |
|---|---|---|---|
| 60 fps | 6,246 | ~104 | ~50 Mbps ✓ |
| 120 fps, before | 12,676 | ~106 | **~100 Mbps** ✗ |
| 120 fps, after | 6,450 | ~54 | **50.7 Mbps** ✓ |

Cause: `EncoderInitParams::framerate(60, 1)` was hardcoded in
`pingpong-encode/src/nvenc.rs`. CBR is a bits-per-*second* budget that NVENC
divides by that number to size each frame, so a stale 60 there does not cap the
bitrate at 120 fps — it doubles it. Fixed by passing the pacer's clamped target
rate through `NvencEncoder::new`, so the encoder is sized against the rate it is
actually fed at. The LAN had the headroom to hide this; a WAN path would not.

## Still not established

**`fec_block_idx` splitting.** A block splits above 200 data shards, i.e. above
~236 KB in one frame. Steady-state frames here are ~45 data shards (~53 KB)
**[V]**, so P-frames do not split and the 255-shard path is still unexercised by
them. Whether a *keyframe* at this mode crosses the threshold was **not
measured** — the per-second stats line does not separate keyframe bytes from
P-frame bytes, and the arithmetic across the session-start second is too noisy to
settle it. Answering it needs an instrument that reports `data_shards` per frame.

**A WAN run at 120 fps.** Everything above is LAN. The 60 fps WAN run's p99 tail
of 35–65 ms was network jitter, and doubling the frame rate halves the time
available to absorb it.

## Defect 3: the host config silently overrode the negotiated rate — since fixed

`target_fps` in the host's `pingpong-server.toml` was the sole input to
`Pacer::new`, and was *not* derived from the mode the client negotiated. A client
that asked for and was granted a 120 Hz virtual display still got 60 fps of video
if the host config said 60 — **silently, with the ack reporting 120.** This run
required setting `target_fps = 120` on the host by hand, and until that was
found, `fps=60` with `dropped_pacer=0` against a confirmed 120 Hz display looked
exactly like the virtual-display fault that plan Task 10 Step 2 warns about.

It looked like nothing was wrong because, from the telemetry's point of view,
nothing was: the pacer was discarding half the frames precisely as configured, so
`dropped_pacer` counts only frames it refused *unexpectedly* and stayed 0.

Fixed after this run (`pingpong-server/src/session.rs`, `encode_fps_for`): the
mode the host actually set now leads, `target_fps` remains a cap for a host that
genuinely cannot sustain its display's refresh rate, and `0` means "no opinion".
The ack now reports **the rate frames will arrive at** rather than the display
mode, so a capped host can no longer promise a rate it will not deliver — spec
§4.4 updated to match. Verified end to end at 3600×2260@60 after the change:
`encode_fps=60`, 60 fps both ends, no drops.

**Consequence for this host:** `target_fps` is back at 60, so a future 120 Hz
request will be capped to 60 — but the ack will now say 60. Set it to `0`, or to
whatever the host can really sustain, for the mode to lead unimpeded.

---

# A mode change under the session, 2026-08-05

**Reported:** changing the display resolution from inside the stream froze the
client and brought the host's physical monitor back on, with the physical
display taking the primary from there. Changing the refresh rate from 60 Hz to
120 Hz also moved the resolution — 3600×2260 → 3840×2160, because the virtual
monitor's mode list at 120 Hz does not contain 3600×2260 — and changing back was
clean. A subsequent change to a *larger* resolution reproduced the freeze.

Read from source, not measured on the host: everything below is **[V]** against
the code or the crate it names, and **[P]** where it says which of them fired on
the reported run. The instrument that would settle the `[P]` is the host log —
`follow_the_desktop` now names each of the three faults separately, so one run
after this change says which.

## Why a frozen client is the symptom of everything here

**[V]** `repeat::HEARTBEAT` is 100 ms and the client's `STALL` is 500 ms, and the
first is deliberately under the second so the client never has to ask for a
keyframe. The consequence was never written down: the idle-repeat thread
re-encodes `EncodeState::last` — a *pointer* to the capture source's interop
texture — and it keeps doing that at 10 fps whether or not capture is still
alive. So a host whose capture has died still emits a decodable stream forever.
The client decodes it, `last_decoded` keeps refreshing, it never requests a
keyframe, and it is never told there is nothing behind the picture.

**Every fault below therefore had to be detected on the host.** Nothing
downstream can see any of them, and no client-side timeout could: a
change-driven capture legitimately sends nothing for minutes (§7.2.1).

## Fault 1 — the session polled the wrong display

**[V]** `current_primary_mode` asked `EnumDisplaySettingsW(NULL, …)` — "the
default display" — on the reasoning that `activate` makes the virtual display
primary. That reasoning holds exactly until the thing it exists to detect
happens. Windows reapplies its own remembered arrangement on a mode change, the
host's monitor comes back at (0,0), and the query then answers about the
*physical* display. The session retunes itself to a display it is not capturing.

Replaced by `mode_for_target(CcdId)`, which asks the session's own display by
CCD identity.

## Fault 2 — re-isolating did not restore the primary, and gave up silently

**[V]** `reassert_isolation` deactivated the other paths and stopped there.
`isolate_to` does not move what it keeps, so a reapply that put the physical
monitor at (0,0) left a single-display desktop whose one display was **not**
primary — `primary_id()` returns `None`, and everything built from "the primary"
is then built against the wrong thing.

**[V]** Worse, when the session's display was not among the active paths at all,
`isolate_to` returned `NoSuchDisplay`, which was logged as a warning — twice a
second, for the rest of the session, while the host's monitors stayed on. That
is the reported "the physical display took the lead from there", and nothing in
the code could ever have taken it back.

`reassert` now asserts both isolation and primary, and returns
`Reassert::DisplayLost` instead of warning, which the session loop can act on.

## Fault 3 — a dead capture was invisible

**[V]** windows-capture 1.5.0 subscribes to `GraphicsCaptureItem::Closed`
(`graphics_capture_api.rs`): the handler sets the halt flag and posts `WM_QUIT`,
the message loop unwinds, the capture thread ends. `CaptureControl::is_finished`
and `halt_handle` both expose that and **neither was read anywhere**. A monitor's
capture item closes when the monitor leaves the desktop, which is what a topology
reapply does to a display while its mode is being changed.

`WgcSource::is_running` reads both. Note what it does *not* do: infer death from
a frame rate. Capture is present-driven, so 0 fps is also what a still desktop
looks like.

## Fault 4 — a rebuilt encoder was built at the old resolution

**[V]** `encode_and_queue` guarded its resolution check on `encoder.is_some()`:

```rust
if encoder.is_some() && (frame.width != *width || frame.height != *height) {
```

which reads as harmless — with no encoder there is nothing to rebuild — and is
not, because `*width`/`*height` are also what the **next** encoder is built at.
Any path that left `encoder` at `None` across a resolution change built the
replacement at the old size and handed it a frame of the new one, which
`NvencEncoder::encode` rejects by contract. The rejection ends the session, the
session restores the desktop, and the host's physical monitor comes back on.

**[V]** There was such a path, and it is precisely the reported 60 → 120 Hz case:
`retune_fps` dropped the encoder for a refresh change, and on this virtual
display a refresh change *is* a resolution change.

## Fault 5 — the client was told a mode that did not exist

**[V]** The refresh-change re-ack sent `width: mode.width, height: mode.height`
— the mode from **before** the change — with the new rate. So a 60 → 120 change
that moved 3600×2260 to 3840×2160 acked "3600×2260 @ 120". A pure resolution
change sent no ack at all: the session loop only compared frame *rates*, and the
resolution was handled down in `encode_and_queue` where nothing can reach the
wire. `SessionState::mode` went stale either way, so a later duplicate
`SessionStart` matched it and was re-acked with geometry that was two changes
out of date.

## The shape of the fix

The half-measure was the problem. `retune_fps` updated the pacer, the idle
repeat and the encoder's CBR budget, and left the ack's geometry, the session's
recorded mode, the absolute-pointer transform and the cursor watcher's scaling
all pointing at the old mode. Everything a session derives from the display mode
now comes from one function (`pipeline::begin`), and a mode change restarts the
stages through it rather than patching four of the eight things.

`DisplayWatch` (in `session.rs`, so it is unit-tested off-host like `repeat`)
turns polls into verdicts. Two properties are load-bearing and each has a test:

- **Edge-triggered on the display's reading**, not level-triggered against the
  session's mode. The two numbers come from different places, and any
  circumstance in which they disagree permanently would rebuild the session
  twice a second forever, re-acking the client each time.
- **Bounded.** `MAX_CONSECUTIVE` polls in a row that all had to act — about 1.5 s
  — ends the session and sends `SessionEnd(NoSession)`, which is the client's
  documented cue to negotiate afresh.

## Also fixed while in there, and not a symptom of the above

**[V]** The NVENC session's destruction thread was decided by `Arc` refcount
order. `EncodeState` is shared by the capture callback and the repeat thread, and
windows-capture's `CaptureControl::stop` drops its own handle on the **caller's**
thread — the session loop, which has never touched CUDA. `NvencEncoder::drop`
with no current context is documented (and measured, 2026-08-05) to hang rather
than fail. The repeat thread now binds and drops the encoder on its way out, and
`RunningSession::stop` joins it first, so the later drop is inert whichever
thread performs it.
