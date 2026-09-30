# Spike: VideoToolbox real-time decode latency

> Historical lab notebook (see [../README.md](../README.md)): `§` references
> are to [v1 design](../../docs/design/v1-design.md); "Task N" to the
> implementation plan of the time, which is in the git history only.

Resolves **risk #3** (plan Task 1): if the decoder buffers frames despite the
real-time flag, spec §12's budget is wrong and the client design changes.

Throwaway code. The deliverable is the numbers below, not the implementation —
Task 14 writes the real decoder.

## Verdict

**PASS.** The real-time flag is honoured, the decoder delivers one callback per
submit with no buffering, and p95 is **1.12 ms** against an 8 ms threshold —
roughly 7× margin. Spec §12's 3–8 ms estimate was conservative. Proceed to
Phase 4 as designed, with **no jitter buffer** (spec §9).

## Measured

Apple Silicon MacBook, macOS 26.5.2 arm64, `--release`. 600 frames of
1920×1080 H.264, no B-frames, infinite GOP (one IDR then 599 P-frames).

| | p50 | p95 | p99 | min | max | max in flight |
|---|---|---|---|---|---|---|
| **Paced 60 fps** (production case) | **0.96 ms** | **1.12 ms** | **2.67 ms** | 0.88 ms | 4.57 ms | **1** |
| Burst (submit as fast as possible) | 3.30 ms | 4.61 ms | 6.58 ms | 1.53 ms | 7.56 ms | 5 |

Both runs: 600/600 frames decoded, **0 decode errors, 0 null image buffers,
0 out-of-order callbacks**.

### `kVTDecompressionPropertyKey_RealTime`

**ACCEPTED** — `VTSessionSetProperty` returned `noErr`. The flag is supported
on this OS/decoder, so Task 14 should set it rather than commenting it out.

### One-per-submit, or bursty?

**One-per-submit.** This is the question the spike existed to answer, and it
needed both runs to answer honestly:

- Paced at 60 fps — the rate the client will actually feed the decoder — the
  in-flight depth never exceeds **1**. Every frame's callback lands before the
  next frame is submitted. No buffering, no reordering.
- The burst run's in-flight depth of 5 is **not** evidence of buffering. It is
  an artifact of submitting 600 frames in ~0.6 s of wall clock: frames pile up
  because submission outruns a ~1 ms decode, not because the decoder is holding
  them back. Reading that number as "bursty" would have been a false alarm.

The paced latency being *lower* than the burst latency (0.96 vs 3.30 ms p50) is
consistent with that reading: under burst, measured latency includes time spent
queued behind other frames, not decode itself.

## Reproducing

```bash
cd spikes/vt-latency

# Generate the test stream: no B-frames (-bf 0) and effectively infinite GOP
# (-g 9999) to mirror the §8.3 NVENC config.
ffmpeg -f lavfi -i testsrc=size=1920x1080:rate=60 -t 10 \
  -c:v libx264 -preset ultrafast -tune zerolatency \
  -bf 0 -g 9999 -x264-params "sliced-threads=0" \
  -f h264 testsrc.h264

cargo run --release
```

`testsrc.h264` is not committed; regenerate it with the command above. Verified
to contain 0 B-frames and 600 frames via `ffprobe`.

## Notes for Task 14

- **The plan's `features = ["all"]` for the `objc2-*` crates does not exist** in
  the 0.3.x line. Those crates gate one feature per framework header; see this
  spike's `Cargo.toml` for the working per-header lists.
- Resolved crate versions: `objc2` 0.6.4, `objc2-core-media` /
  `objc2-core-video` / `objc2-video-toolbox` / `objc2-core-foundation` all
  0.3.2.
- `VTSession` is a type alias for `CFType`, and `VTDecompressionSession` derefs
  to `CFType`, so `VTSessionSetProperty(&session, ...)` works by deref coercion.
- VideoToolbox rejects Annex-B. Each frame is submitted as AVCC: the start code
  replaced by a 4-byte big-endian length, matching the
  `nal_unit_header_length = 4` passed to
  `CMVideoFormatDescriptionCreateFromH264ParameterSets`.
- `CMBlockBuffer::create_with_memory_block` is given `kCFAllocatorNull` as the
  block allocator so it borrows the frame buffer without copying or freeing it.
  That requires the backing `Vec` to outlive the sample buffer — the spike
  builds all frames up front for that reason. **Task 14 must not copy this
  pattern naively**, since in production frames arrive and are dropped
  continuously; either let CoreMedia own the memory or tie the lifetime
  explicitly.
- Submissions are busy-waited rather than `sleep`-paced, because sleep
  granularity would add jitter to a measurement in the low milliseconds.
