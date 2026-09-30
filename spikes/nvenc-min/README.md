# Spike: minimal NVENC P1 ultra-low-latency encode

> Historical lab notebook (see [../README.md](../README.md)): `§` references
> are to [v1 design](../../docs/design/v1-design.md); "Task N" to the
> implementation plan of the time, which is in the git history only.

Resolves **risk #5** (plan Task 3): the crate's init path is less trodden than
the C samples, so prove the init + config sequence before entangling it with
capture.

Throwaway code. The deliverables are the findings below and the exact working
field paths, which Task 12 needs verbatim.

## Verdict

**Proceed.** Init, config and encode all work, the latency-critical settings take
effect, and **submit→available is 2.37 ms p50** — inside §12's 3–6 ms estimate.
Two non-obvious requirements had to be discovered; both are recorded below and
both bite Task 12.

## Measured

RTX 3070 Ti, driver 610.47, CUDA 12.6. 120 frames of synthetic 1920×1080 NV12,
H.264 High, P1 preset + `ULTRA_LOW_LATENCY` tuning, CBR at 20 Mbps target.

| | cold | p50 | p95 | p99 | min | max |
|---|---|---|---|---|---|---|
| submit → bitstream available | 3.39 ms | **2.37 ms** | 2.45 ms | 2.53 ms | 2.03 ms | 3.39 ms |

Timing starts after upload and registration, so it is encode only.

**Zero B-frames confirmed** via `ffprobe`: 0 B, 1 I, 119 P over 120 frames. Both
latency-critical settings therefore took effect — `frameIntervalP = 1` (no
reorder delay) and `gopLength = NVENC_INFINITE_GOPLENGTH` (a single IDR at the
start, no periodic keyframes).

Output: 3 200 829 bytes for 120 frames = 12.8 Mbps at 60 fps, under the 20 Mbps
CBR target, as expected for a trivially compressible synthetic gradient.

## Finding 1: NVENC cannot register cudarc-allocated memory

**`stream.alloc_zeros()` memory fails `register_generic_resource` with
`ResourceRegisterFailed`, at every pitch.**

cudarc's `alloc`/`alloc_zeros` call **`cuMemAllocAsync`** whenever the device
supports it — gated on `CudaContext::has_async_alloc`, which is `pub(crate)` with
no opt-out. That returns *stream-ordered* memory from a CUDA memory pool, and
NVENC cannot register pool memory.

Allocating the same bytes with plain **`cuMemAlloc`**
(`cudarc::driver::result::malloc_sync`) registers immediately.

The probe tests four pitches on each allocator, which is what makes the
attribution solid:

```
[A]  cudarc alloc_zeros, pitch  1920: FAILED ResourceRegisterFailed
[A]  cudarc alloc_zeros, pitch  2048: FAILED ResourceRegisterFailed
[A]  cudarc alloc_zeros, pitch  2560: FAILED ResourceRegisterFailed
[A]  cudarc alloc_zeros, pitch  4096: FAILED ResourceRegisterFailed
[A2] cuMemAlloc,         pitch  1920: SUCCESS
```

**Alignment was a red herring.** The first hypothesis was that 1920 is a
multiple of 4 but not of 256, and that NVENC wanted `cuMemAllocPitch`-style
padding — which is why the probe sweeps pitches at all. It does not: the natural
pitch of 1920 works fine once the allocator is right. Had the probe tested only
one allocator at one pitch, the wrong conclusion was readily available.

**Task 12 must allocate its NV12 planes with `result::malloc_sync` and free them
with `result::free_sync`**, not with `alloc_zeros`. Those buffers are both the
conversion kernel's output and NVENC's input, so the raw `CUdeviceptr` is passed
as a kernel argument — which works, since the kernel takes plain
`unsigned char*`.

## Finding 2: picture-type decision must be enabled

Without it, `encode_picture` fails with
`InvalidParam: "Invalid value for picture type."`, because
`EncodePictureParams::default()` carries `NV_ENC_PIC_TYPE_UNKNOWN` and NVENC
rejects UNKNOWN when it is not deciding picture types itself.

`EncoderInitParams::enable_picture_type_decision()` fixes it, and is what we
want anyway: the encoder emits one IDR then P-frames indefinitely, with
keyframes only on demand.

## The working sequence (for Task 12)

```rust
let ctx = CudaContext::new(0)?;
let encoder = Encoder::initialize_with_cuda(ctx.clone())?;

// Assert support before configuring.
encoder.get_encode_guids()?.contains(&NV_ENC_CODEC_H264_GUID)
encoder.get_preset_guids(NV_ENC_CODEC_H264_GUID)?.contains(&NV_ENC_PRESET_P1_GUID)
encoder.get_supported_input_formats(NV_ENC_CODEC_H264_GUID)?
       .contains(&NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12)

let mut preset = encoder.get_preset_config(
    NV_ENC_CODEC_H264_GUID,
    NV_ENC_PRESET_P1_GUID,
    NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
)?;
let cfg = &mut preset.presetCfg;              // <-- the field paths that work
cfg.frameIntervalP = 1;
cfg.gopLength = NVENC_INFINITE_GOPLENGTH;
cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
cfg.rcParams.averageBitRate = BITRATE;

let mut init = EncoderInitParams::new(NV_ENC_CODEC_H264_GUID, WIDTH, HEIGHT);
init.preset_guid(NV_ENC_PRESET_P1_GUID)
    .tuning_info(NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY)
    .framerate(60, 1)
    .enable_picture_type_decision()           // <-- required, see Finding 2
    .encode_config(cfg);

let session = encoder.start_session(NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12, init)?;
ctx.bind_to_thread()?;                        // context must be current to register
```

Per frame: `register_generic_resource(marker, CUDADEVICEPTR, dptr, pitch)` →
`create_output_bitstream()` → `encode_picture(...)` → `bitstream.lock()` (which
blocks until available) → `locked.data()`.

Task 12 should register its persistent NV12 buffers **once** rather than per
frame as this spike does, and keep at most `max_in_flight` (default 2) frames
outstanding rather than serialising submit→wait→submit (§7.3). This spike
deliberately serialises, so 2.37 ms is a *serialised* figure — pipelining should
hide most of it.

## Build requirements

- **`cudarc` 0.16**, not the plan's 0.19: `nvidia-video-codec-sdk` 0.4.0 depends
  on `cudarc ^0.16.4` and takes `Arc<CudaContext>`, so the versions must match or
  the context cannot be handed to the encoder at all. See
  `spikes/cuda-interop/README.md`.
- **`nvEncodeAPI.lib` and `nvcuvid.lib` are required and are NOT in the CUDA
  toolkit** — they ship only with NVIDIA's login-gated Video Codec SDK. Generate
  them from the driver's own DLLs instead:

  ```powershell
  powershell -ExecutionPolicy Bypass -File tools\gen-nvenc-import-libs.ps1
  # then, persisted for this user already:
  $env:NVIDIA_VIDEO_CODEC_SDK_PATH = "$env:LOCALAPPDATA\pingpong\nvenc-libs"
  ```

## Reproducing

Unlike the capture spikes, this one needs **no** interactive desktop session —
NVENC does not touch the desktop, so plain SSH is fine.

```powershell
cd C:\path\to\pingpong\spikes\nvenc-min
cargo build --release
.\target\release\nvenc-min.exe

ffprobe -hide_banner -show_frames -select_streams v out.h264 | Select-String "pict_type=B"
# expected: no matches
ffplay out.h264   # a moving diagonal gradient, 120 frames
```

`out.h264` is not committed (see `.gitignore`).
