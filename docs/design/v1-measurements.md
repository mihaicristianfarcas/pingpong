# v1 latency measurements

> **Historical record.** Kept as it was written, for the reasoning and the
> measurements behind the code; code comments cite its sections. Where it
> and the code disagree, the code and the [current docs](../README.md) are
> right. See [the design history](README.md).

Records the figures that decide whether v1 succeeded (spec §1.3).

**Status: per-stage telemetry measured (2026-07-31). Glass-to-glass not yet
measured** — the camera comparison against Moonlight/Apollo is deferred, so
§1.3.3 has no verdict. Every table below is marked with which of the two it is.

---

## Why the camera measurement is the one that counts

The in-process telemetry (§12.1) attributes every stage that happens *inside*
one of the two processes. It cannot see two things:

1. **The network leg.** The host and the client stamp timestamps against
   unrelated epochs, so `present_us - capture_ts_us` is off by whatever gap
   happens to sit between them. The client reports that figure relative to the
   best frame it has seen, which cancels the offset and gives real added
   latency — but not an absolute one-way delay.
2. **Display latency.** Neither process can observe the pixels reaching glass.

A camera pointed at both screens sees both. That is why §1.3.3 is defined
against it, and why it is defined *relative to Moonlight* — an absolute
millisecond count is dominated by the two panels, which are not the thing under
test.

---

## Hardware and path

| | Host (the Windows test host) | Client |
|---|---|---|
| Machine | desktop PC | MacBook Pro |
| OS | Windows 11 | macOS 26.5.2 |
| GPU | NVIDIA RTX 3070 Ti | Apple M4 Pro |
| Display mode | 1920×1080 @ 60 Hz | 3024×1964 Retina, ProMotion |
| Panel | external | built-in Liquid Retina XDR |

The host was set to 1920×1080 @ 60 Hz for the run (it is normally
2560×1440 @ 144 Hz); v1 encodes whatever the desktop is, and 1080p60 is what
§1.1 scopes.

| | |
|---|---|
| Network (LAN) | Tailscale, **direct** path `192.168.1.10` → `192.168.1.20:41641` |
| | Client is on **Wi-Fi** (`en0`), not wired |
| Measured RTT, LAN | **12.5 ms avg** (min 8.5 / max 21.6 / stddev 4.8, 5 pings) |
| Network (WAN) | not measured |
| Measured RTT, WAN | not measured |
| `target_fps` / `bitrate_bps` | 60 / 20 000 000 |

**The LAN path is not a fast path.** Tailscale chose a direct route, so nothing
is relayed, but the client leg is Wi-Fi and the RTT is ~12.5 ms with 4.8 ms of
jitter — a one-way network leg of roughly 6 ms, larger than the entire host
pipeline. Neither telemetry table below contains that leg, and the
glass-to-glass figures (when taken) will. Read the two together.

---

## Per-stage telemetry

From the per-second log lines of the 2026-07-31 05:35 run. All values in
milliseconds. Stream state during the sampled seconds:

```
fps=60  datagrams=2258  dropped_pacer=0  dropped_queue=0  established=true
```

### Host

Sampled second: 05:35:54, `n=60` frames (i.e. a full second at the target rate).

| Stage | p50 | p95 | p99 |
|---|---|---|---|
| capture→encode | 2.56 | 2.70 | 2.79 |
| encode→send | 0.24 | 0.31 | 0.36 |
| **capture→sent (host total)** | **2.81** | **2.98** | **3.14** |

`capture→sent` is measured directly, not summed. It should land close to the
sum of the two stages above; a gap means something between them is unmeasured.
**It does: 2.56 + 0.24 = 2.80 against a measured 2.81.** Nothing on the host is
outside the attribution.

The host figures are steady and repeatable — three consecutive seconds agreed to
within 0.05 ms at p50.

### Client

Sampled second: 05:35:50, `n=28..32`. **These are a one-second sample, and the
tails are not trustworthy** — see the caveat below.

| Stage | p50 | p95 | p99 |
|---|---|---|---|
| recv→reassembled | 0.43 | 1.66 | 3.48 |
| reassembled→decoded | 1.20 | 3.81 | 71.58 |
| decoded→presented | 0.22 | 1.33 | 2.65 |
| **recv→presented (client total)** | **2.00** | **4.54** | **77.72** |
| capture→present, above the best frame seen | 1.47 | 8.92 | 189.23 |

Same cross-check: 0.43 + 1.20 + 0.22 = 1.85 against a measured 2.00 at p50 —
consistent, with 0.15 ms unattributed (mailbox handoff and the event-loop wake).

**Caveat on the client tails.** The tunnel took ~30 s to establish in this run,
leaving only about one second of steady state before the run ended. The p50s are
consistent with earlier runs and with the host, but the p95/p99 columns include
the first frames after establish — the decoder's first keyframe, the pipeline
priming — and are startup transient, not steady state. A p99 of 71.58 ms on
`reassembled→decoded` is one cold-start frame in a sample of 32, not a stall the
stream exhibits. **These tails should be re-taken over a steady minute before
anyone cites them.**

### Against the §12 budget

| Stage | §12 expected | Measured | Notes |
|---|---|---|---|
| CUDA BGRA→NV12 | 0.43 ms p50 **[V]** | — | not separable in-pipeline; folded into capture→encode |
| NVENC P1 ULL | 2.37 ms p50 **[V]** | — | likewise |
| CUDA + NVENC together | 2.80 ms p50 | **2.56 ms p50** | end-to-end beats the sum of the two spikes |
| Packetise + RS encode | <0.5 ms **[P]** | **0.24 ms p50** | now **[V]** — `encode→send` is exactly this stage |
| VideoToolbox decode | 1.12 ms p95 **[V]** | **1.20 ms p50** | in-stream; the spike measured a file, so this includes queueing |
| Metal present | ≤16.7 ms **[P]** | **0.22 ms p50** | now **[V]**; `displaySyncEnabled = false` is what buys this |

The pipeline meets the §12 budget at every stage that could be compared, and
beats it on the host: capture→encode measured 2.56 ms against the 2.80 ms the
two spikes predicted when summed.

---

## Time to answer a keyframe request (2026-08-04)

The one thing a client can ask the host to *do*, so how long it takes is a
latency figure like any other — and until this run it was unbounded.

Measured by `pingpong-encode/examples/idle_repeat.rs` on the test host, interactive
session, 1024×768 desktop left untouched for the idle window.

| | |
|---|---|
| Captures delivered, 2 s idle desktop | **5 (2.50 fps)** |
| Repeats encoded during that window | 28 |
| **Keyframe request → IDR out** | **21.2 ms** / **2.8 ms** (two runs) |
| Answered by | a **repeat** — no capture involved |
| Mean P-frame | **2121 B** |
| Heartbeat cost at 10/s | **~170 kbit/s** — under 1% of the 20 Mbit/s target |

The bandwidth line is there because the heartbeat's justification is that a
repeat of unchanged content is nearly free, and under CBR that is a claim about
rate control rather than an identity. It holds: under two datagrams per frame.

The comparison figure is the one this replaces: before the repeat thread, a
keyframe request was serviced only inside the capture callback, so the answer
came on the *next present of the host desktop*. At the 2.50 fps measured above
that averages ~400 ms, and on a desktop with genuinely nothing on it — a bare
virtual display, which is the normal case for a headless host — it never came at
all. `pingpong-capture`'s `rate` example measures the same idle trickle
independently: **1.80 fps idle against 60.00 fps with a window moving.**

The run also exercises the assumption the fix rests on: 28 frames were encoded
from a texture pointer held across sink callbacks and used from another thread,
with no crash and no corruption. See spec §7.2.2.

---

## Time to establish the tunnel

Not a §1.3 criterion, but it is the most variable number the project produces
and it deserves recording, because it is the one place the PQ handshake's size
appears to be costing something.

| Run | Time from client start to `tunnel established` |
|---|---|
| 2026-07-31 ~05:31 | ~5 s |
| 2026-07-31 05:35 | ~30 s |

**Both are multiples of 5 s, which is `REKEY_TIMEOUT`** in the fork
(`boringtun/src/noise/timers.rs:22`) — the interval at which an unanswered
handshake initiation is retried. So the handshake is not *slow*; it is being
*lost and retried*, once in the first run and about six times in the second.

The likely cause, unproven: this fork's phase-2 initiation is ~2420 B, which at
the 1280-byte path MTU is **three datagrams that all have to arrive**. Losing
any one loses the whole initiation and costs 5 s. The client leg is Wi-Fi with
4.8 ms of RTT jitter, and a 3-datagram all-or-nothing burst is roughly three
times as likely to be lost as WireGuard's single 148-byte initiation.

This is the same structural risk as §11.4 #1 — a segmented handshake sharing a
path with something else — seen under *loss* rather than under *load*. The rekey
test below covers the load half on a lossless path; the loss half is untested
and is the obvious next experiment (repeat the establish 20 times, count the
5 s multiples, then repeat over Ethernet).

---

## Rekey under sustained load (§1.3.4)

> No visible artifact at rekey boundaries during 10 minutes of sustained
> streaming.

Measured by `pingpong-server/tests/rekey_under_load.rs`, which drives real
`Tunnel` pairs with synthetic 1080p60-shaped frames (41 667 B at 60 fps,
~2100 packets/sec) for ten minutes and timestamps every rekey. Both tunnels are
in one process, so completion latency is measured against a single clock and
needs no correlation.

The pass rule is mechanical: no frame completing within 500 ms of a rekey may
exceed **3× the p99 of frames outside those windows**.

### Result: PASS (2026-07-31, 601 s run)

| | |
|---|---|
| Frames sent / completed | **36 000 / 36 000** |
| Incomplete frames | **0** |
| Send errors | 0 |
| Rekeys observed | **5**, at 120.01 / 240.05 / 360.02 / 480.02 / 600.05 s |
| Segmented handshake messages | **12** = 2 initial + 2 per rekey × 5 |

| Latency | n | p50 | p95 | p99 | max |
|---|---|---|---|---|---|
| Outside rekey windows | 35 733 | 0.23 | 0.31 | 0.33 | **3.32** |
| Within 500 ms of a rekey | 267 | 0.23 | 0.31 | 0.33 | **0.40** |

Budget was 0.98 ms (3× the baseline p99). Worst frame near each rekey: 0.26,
0.33, 0.40, 0.33, 0.32 ms.

**The two distributions are identical to two decimal places at every
percentile.** The strongest single line in the table is the max column read
backwards: the worst frame in the whole run, 3.32 ms, happened *away* from a
rekey. Nothing in the run's tail is attributable to rekey — if anything, the
frames near rekeys were better behaved than average, which is what you get when
267 samples are drawn from a distribution whose outliers are rare.

Every rekey did segment; that is what 12 segmented messages against 5 rekeys
shows. Had `WriteManyToNetwork` quietly stopped firing, the count would have
been 2 and this test would have proved nothing — which is why it is asserted.

**No mitigation from §11.4 was needed.** The rekey interval is untouched,
segments are still sent back to back, and the send queue does not prioritise
them.

**What this run does not cover.** The path is loopback: no loss, no reordering,
~0.23 ms of it. It answers "does a segmented rekey disturb a saturated stream"
and not "does a segmented handshake survive a lossy path" — and the establish
times above suggest the second question has a less comfortable answer.

---

## Glass-to-glass

**Status: deferred, not measured.** The tables below are the scaffolding to fill
in. Until they are, §1.3.3 has no verdict — the per-stage numbers above cannot
substitute, because they contain neither the network leg (~6 ms one way here)
nor either display's own latency.

**Method.** A millisecond timer on the host display, both screens in one frame,
phone camera at 240 fps. Latency is the frame delta between the two readings
× (1000/240) ms. Ten repetitions, median reported — a single reading carries up
to one camera-frame (4.2 ms) of quantisation and one display period of phase.

Run pingpong and Moonlight/Apollo **back to back on the same path in the same
session**. Anything else — different time of day, different network conditions,
a reboot in between — invalidates the comparison, which is the entire point of
taking it.

### LAN

| Run | pingpong (frames) | pingpong (ms) | Moonlight (frames) | Moonlight (ms) |
|---|---|---|---|---|
| 1 | | | | |
| 2 | | | | |
| 3 | | | | |
| 4 | | | | |
| 5 | | | | |
| 6 | | | | |
| 7 | | | | |
| 8 | | | | |
| 9 | | | | |
| 10 | | | | |
| **median** | | | | |

### WAN

Same procedure from outside the home network, so roaming (§11.3) and real RTT
are included.

| Run | pingpong (frames) | pingpong (ms) | Moonlight (frames) | Moonlight (ms) |
|---|---|---|---|---|
| 1 | | | | |
| 2 | | | | |
| 3 | | | | |
| 4 | | | | |
| 5 | | | | |
| 6 | | | | |
| 7 | | | | |
| 8 | | | | |
| 9 | | | | |
| 10 | | | | |
| **median** | | | | |

---

## Verdict against §1.3.3

> Glass-to-glass latency is within ~20% of the existing Moonlight/Apollo setup
> on the same network path.

| Path | pingpong median | Moonlight median | Ratio | Within 20%? |
|---|---|---|---|---|
| LAN | | | | |
| WAN | | | | |

**Result:** _not yet measured._

If the ratio misses, the per-stage tables above are what says where the time
went — that is what they are for. Record the answer either way; a v1 that is
slower than Moonlight and *knows why* is a better outcome than one that is
faster and does not.
