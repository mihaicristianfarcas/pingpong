# pingpong v2 Phase 2 — Input: implementation design

> **Historical record.** Kept as it was written, for the reasoning and the
> measurements behind the code; code comments cite its sections. Where it
> and the code disagree, the code and the [current docs](../README.md) are
> right. See [the design history](README.md).

**Status:** approved design, not yet implemented
**Date:** 2026-08-01
**Scope:** Phase 2 of [v2-design.md](v2-design.md), referenced throughout
as "v2 §n".

**This document does not restate v2 §5.** That section is the design for input
and remains the source of truth for the wire format, the send discipline, the
healing scheme and the mouse-mode rationale. What follows is the layer below it:
which crates and modules exist, what types they expose, what the threads
actually do, and the handful of decisions phase 2 had to make that v2 did not
settle. Where brainstorming found v2 wrong or silent, **v2 was corrected in
place** — see §7 for the list. Nothing is worked around locally.

Same two tiers as v1 and v2, marked inline: **[V]** verified against source,
measurement or a cited artefact; **[P]** provisional, a hypothesis to confirm.

---

## 1. What phase 2 delivers

Keyboard and mouse, relative and absolute, hotkey-toggled, healed against burst
loss — v2 §5 in full. Plus display-sleep inhibition (v2 §6.5), which is host
session behaviour with no other phase to belong to and is currently the only
known way a healthy session dies.

**Explicitly not in phase 2:** success criterion 2 (streaming above 60 fps).
That is phase 1 debt, recorded in [v2-measurements.md](v2-measurements.md) and in the
phase 0–1 plan's Task 10, and it drags encoder tuning — `max_in_flight`, the
255-shard FEC path — into a plan that is otherwise about input. It gets its own
follow-up.

**Also not in phase 2:** `HeldState` (v2 §4.3 tag 7, §5.4). It stays reserved
and unimplemented. §5's acceptance test is what decides whether it is ever built.

---

## 2. Crate and module layout

```
pingpong-proto/src/input.rs    InputEvent · Key · scancode table · InputRing ·
                               SequenceGate · MotionBatcher · RepeatSchedule   NEW module
pingpong-input/                trait InputSink → SendInput impl                NEW crate
  src/lib.rs                     platform-free: trait, InputEvent re-export, errors
  src/windows.rs                 #[cfg(windows)] SendInput + held-set tracking
pingpong-client/src/input.rs   KeyCode → Key, winit → InputEvent,
                               grab, hotkeys, send thread                     NEW module
```

`pingpong-input` deliberately mirrors `pingpong-display`'s shape — platform-free
`lib.rs` over a `#[cfg(windows)]` implementation module — because that layout is
already proven in this workspace and because v1 §15.6 named injection and
display control as the same kind of seam.

Per v2 E9 the client gets no crate. It gains a module: `pingpong-client/src/main.rs`
is 963 lines **[V]** and v2 §3.1 already requires the split before input is added
to it.

`pingpong-proto`'s constraint is unchanged and binds `input.rs`: **no dependency
on `pingpong-transport`, any GPU crate, or any platform crate.** Every type in §3
is pure and testable with no winit, no Windows and no sockets.

---

## 3. The pure core: `pingpong-proto::input`

All the logic that can be wrong lives here, which is what makes §5's test plan
possible without hardware.

### 3.1 Types

```rust
pub enum InputEvent {              // v2 §4.3 tags 0..=6; tag 7 HeldState unused
    KeyDown(u16), KeyUp(u16),      // set-1 scancode, E0 extended in the high bit
    MouseMoveRel { dx: i16, dy: i16 },
    MouseMoveAbs { x: u16, y: u16 },
    ButtonDown(Button), ButtonUp(Button),
    Wheel { dv: i16, dh: i16 },
}

pub enum Button { Left, Right, Middle, X1, X2 }   // wire 0..=4, v2 §4.3
```

### 3.2 `InputRing` — client side

Holds the last 8 events and `next_seq: u32`. `encode()` writes the v1 §5 header
through `Header::encode` — kind=2, FEC fields zeroed, `frame_id` = the sequence
of the **newest** event, `capture_ts_us` = that event's client time (v2 §4.1) —
then `event_count`, then the records.

**Every packet carries the whole ring, not just what is new.** That is what makes
v2 §5.4's redundancy free: no per-event sequence number is transmitted, the
receiver derives the span from `frame_id - count + 1 ..= frame_id`, and an
isolated loss self-heals from the next packet at zero extra cost.

### 3.3 `SequenceGate` — host side

Keeps `last_applied_seq: u32` and returns only the suffix of a packet above it.
Comparison is wrap-aware — `(a.wrapping_sub(b) as i32) > 0`, per v1 §5.1 — which
is required by the spec regardless of the fact that u32 at ~1000 pps takes 49
days to wrap.

Duplicates and reorders are therefore discarded **by construction**, including
relative deltas, which are never double-applied despite being retransmitted.
This is why v2 §5.4's trailing repeat needs no host-side code at all.

### 3.4 `MotionBatcher`

Accumulates relative motion over v2 §5.3's ≤1 ms window and **splits rather than
saturates** at `i16` bounds — **[V]** Moonlight splits for exactly this reason,
and saturating would silently clamp a fast flick.

It accumulates in `f64` and **carries the fractional residue** across batches:
`DeviceEvent::MouseMotion` deltas are `f64` "in unspecified units" **[V]**, and
truncating each one independently means a 0.4-px-per-event slow drag rounds to
zero forever. This is v2 §4.3's new rounding rule.

### 3.5 `RepeatSchedule`

v2 §5.4's 1/2/4/8/16/32/64/128 ms inter-send gaps as a pure iterator:
`next_delay() -> Option<Duration>`, `reset()`. No timer, no thread, no clock — it
is driven by whatever calls it, which is what makes the schedule testable.

### 3.6 The scancode table

Two pieces, and the split is what keeps the table pure.

`pingpong-proto` cannot depend on `winit` (§2), so a table keyed directly on
`KeyCode` could not live in the pure core. Instead:

- **`pingpong-proto::input::Key`** — one variant per physical key this protocol
  can express, named by its **W3C UI Events `code` value**. A published standard,
  not a shadow of any one windowing library, which is precisely what makes it
  legitimate in a crate that must not know what winit is. winit's `KeyCode`
  implements the same standard, so the mapping below is 1:1 by name.
- **`pingpong-proto::input::scancode(Key) -> u16`** — the table. **Total**, with
  no `Option` and no fallback: a key with no set-1 representation simply has no
  `Key` variant.
- **`pingpong-client::input::to_key(KeyCode) -> Option<Key>`** — the winit
  boundary, and the one place v2 §5.1's deliberate drop happens.

**Exhaustive match with no catch-all arm** in `to_key`: every `KeyCode` winit can
emit is either mapped or named in an explicit unmapped list, so a winit upgrade
that adds a variant fails to compile instead of silently producing a wrong
keypress — which §5.1 is explicit is worse than producing none.

Two matches rather than one, deliberately. What it buys: the interesting half —
which scancode a key actually has — stays pure and testable with no winit; the
"what about keys we cannot express" question is answered once, at the boundary,
instead of on every lookup; a future non-winit client reuses the table rather
than reimplementing it. It also buys a test the single-match version could not
have — because `scancode` is total and `Key::ALL` enumerates its domain,
**scancode collisions are checked exhaustively** rather than over a hand-picked
sample, and a collision is exactly the kind of bug that stays invisible until a
player rebinds a key and gets the wrong action.

---

## 4. The two edges

### 4.1 Client — `pingpong-client/src/input.rs`

**Thread shape.** winit's `ApplicationHandler` pushes events into an unbounded
`crossbeam-channel`; one send thread blocks on it (v2 E7, §11).

*Unbounded, deliberately.* v1 §10.1's bounded-at-2 drop-oldest rule is for media
and v2 §11 forbids applying it here — dropping the oldest input is dropping a
keystroke. Nor can it be bounded-and-blocking: the producer is the winit main
thread, which also presents frames. The queue cannot actually grow, because the
producer is a human at ≤1000 events/s and the consumer is one syscall.

**The loop is a single `recv_timeout`.** The ≤1 ms batching window and the
trailing-repeat backoff are the same timeout, so phase 2 adds no timer thread:

```
recv()                    → an event. If relative motion, recv_timeout(1ms)
                            accumulating deltas. Keys and buttons send at once —
                            batching a keypress adds latency for nothing (v2 §5.3).
                          → push to ring, send, RepeatSchedule::reset()

recv_timeout(next_delay)  → Ok(ev):  a new event; cancel the schedule, since the
                                     fresh packet carries the same ring
                          → Timeout: resend the ring, advance the schedule
```

Re-sends are **byte-identical**, `capture_ts_us` included (v2 §5.4, as corrected).

**Relative motion comes from `DeviceEvent::MouseMotion`; absolute from
`WindowEvent::CursorMoved`.** **[V]** and non-interchangeable — see v2 §5.2 as
corrected.

**Absolute coordinates are transformed client-side**, window → stream pixels
(v2 §5.2). This is not skippable despite fullscreen 1:1 being the intended usage:
`request_inner_size` is a *request*, so the window is not guaranteed to be exactly
the acked mode, and `pingpong-client/src/main.rs:461` already treats it as such.

**Grab and mode.** Relative = `CursorGrabMode::Locked` + `set_cursor_visible(false)`;
absolute = no grab. Cursor hidden in both. Session opens in absolute. All four
per v2 §5.2.

**Hotkeys** are intercepted and never forwarded: `Ctrl+Alt+Shift+M` toggles mode,
`Ctrl+Alt+Shift+Q` ends the session (v2 §5.2).

**[P] macOS may require Input Monitoring permission for `DeviceEvent` delivery**
when the window is not focused. v2 §5.1 already commits to degrading gracefully
rather than demanding permissions; confirm during implementation and document
whatever is true.

### 4.2 Host — `pingpong-input`

```rust
pub trait InputSink {
    fn inject(&mut self, events: &[InputEvent]) -> Result<(), InputError>;
    fn release_all(&mut self) -> Result<(), InputError>;
}
```

The Windows implementation **tracks the held key and button set**, so
`release_all()` on teardown is real rather than hopeful. It is the backstop for
v2 §5.4 and the answer to v2 §9's "client dies holding a key".

Keys inject with `KEYEVENTF_SCANCODE` and the E0 bit becomes
`KEYEVENTF_EXTENDEDKEY`, with the layout tradeoff v2 §5.1 states explicitly and
accepts.

**Dispatch** is a new `Kind::Input` arm in `handle_inner`
(`pingpong-server/src/stages.rs:295` **[V]**), replacing the current
`other => ignoring inbound packet kind` catch-all. Injection runs on the receive
thread directly — v2 §11: one `SendInput` syscall does not merit a hop.

**Input is never a liveness signal.** v2 §7 — silence is normal, the user is not
always touching anything. Nothing in this module feeds session state.

### 4.3 Display-sleep inhibition

Lands in `pingpong-display`'s **existing** `vdd-keepalive` thread
(`pingpong-display/src/windows.rs:352` **[V]**), which asserts
`ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED` on entry and clears
with `ES_CONTINUOUS` on exit.

That thread and not a new one: `SetThreadExecutionState` flags are per-thread, so
the inhibit must live somewhere whose lifetime is exactly the session's — and
that thread is started by `activate` and stopped by `restore` **[V]**. The
lifetime is correct for free. Full rationale and the measurement in v2 §6.5.

---

## 5. Testing

### 5.1 Pure, in `pingpong-proto` — no sockets, no GPU, no Windows

Per v1 D9 and v2 §10.1:

- Scancode table exhaustive-match (§3.6) and round-trip.
- Input batch and control encode/decode round-trip, including sequence wrap.
- `MotionBatcher` splits rather than saturates at `i16` bounds; fractional
  residue is carried, so N slow sub-pixel events move the cursor.
- **The invariant property test.** Under arbitrary drop / reorder / duplication,
  the sequence of events the receiver *applies* equals the lossless sequence.
  v2's analogue of v1 §13.1.
- Trailing-repeat schedule: a single surviving copy at any point in the backoff
  releases a held key.
- **Fuzz target** on the input parser, extending v1's depacketiser fuzz. It
  parses hostile network input too.

### 5.2 Hardware

A **Windows raw-input probe** that reads raw input and reports what it actually
saw (v2 §10.2). Not optional: "SendInput returned success" is not evidence a game
saw anything, and v1 §8.3.5 spent 300 frames on a `request_keyframe` that
returned success and did nothing.

Everything display- or input-related runs in the **interactive desktop session**
via `spikes/run-interactive.ps1`. SSH lands in session 0, where this does not work.

### 5.3 The acceptance test (v2 §10.3)

> 20 disconnect/reconnect cycles, each holding at least one key across the
> disconnect, over an input path with induced burst loss, leave zero stuck
> inputs and the host display exactly as found.

Burst loss is induced by a **client-side drop injector on `kind=2` only**, behind
a debug config flag, discarding in deterministic 20–200 ms bursts that straddle
the 255 ms trailing-repeat window on both sides. Uniform per-packet drop is the
case v2 §5.4 handles trivially, so testing it proves nothing.

*Why client-side rather than real impairment.* Only kind=2 is affected, so video
stays clean and a stuck key is unambiguous rather than competing for attention
with a visibly broken stream. It is deterministic, reproducible, and needs no
sudo. **Recorded limitation:** it drops before encryption, so it exercises
§5.4's healing and not the tunnel. That is the right target — §5.4 is a
healing scheme, not a transport.

This is the test that decides whether `HeldState` gets built.

---

## 6. Build order

v2 §12: relative → absolute → the toggle. The pure core (§3) comes first because
everything else consumes it, and it is the part that can be finished and proven
before any hardware is involved.

Display-sleep inhibition (§4.3) is independent of all of it and can land at any
point.

---

## 7. Corrections made to the v2 spec

Brainstorming this phase surfaced four problems in [v2-design.md](v2-design.md).
All four were **fixed in that document**, per its own standing instruction that a
wrong `[P]` is corrected in place rather than worked around locally:

| # | Problem | Fix |
|---|---|---|
| 1 | §10.1 demanded a scancode for *every* `KeyCode`, contradicting §5.1's deliberate dropping of unmapped keys | §10.1 now specifies exhaustive **match**, not totality — mapped or explicitly listed, no catch-all |
| 2 | §4.3 specified `i16` deltas but named no rounding policy for winit's `f64` | §4.3 gains the fractional-residue rule |
| 3 | §5.4 did not say whether a trailing repeat is byte-identical | §5.4 states that it is, and why restamping `capture_ts_us` would corrupt latency figures |
| 4 | Display sleep — the only known way a healthy session dies — appeared only in a measurements file, and in no design section | New §6.5, a §9 error-table row, a §12 phase-2 entry, and an appendix claim |

§5.2 additionally gained the `DeviceEvent::MouseMotion` vs `CursorMoved`
distinction and the absolute-mode session default, and §14's appendix gained
three verified claims.
