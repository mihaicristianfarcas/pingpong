# Spike: WGC texture → CUDA → NV12

> Historical lab notebook (see [../README.md](../README.md)): `§` references
> are to [v1 design](../../docs/design/v1-design.md); "Task N" to the
> implementation plan of the time, which is in the git history only.

Resolves **risk #2**, the highest-uncertainty item in the project (plan Task 2),
and incidentally settles **risk #6**. Implements spec §8.4.

Throwaway code. The deliverables are the findings below and the four
`extern "C"` declarations in `src/main.rs` that Task 12 reuses.

## Verdict

**Proceed, but §8's zero-copy premise is wrong.** The interop path works and the
NV12 conversion is visually correct. However, a WGC pool texture **cannot** be
registered with CUDA at all — a per-frame `CopyResource` into a texture we
create is mandatory. §8's design survives; its main stated advantage does not.

## Findings

### 1. Direct registration of a WGC texture FAILS

```
cuGraphicsD3D11RegisterResource(&res, wgc_texture, NONE)
  -> CUDA_ERROR_INVALID_HANDLE
```

The texture handed out by `Direct3D11CaptureFramePool` is rejected. This is not
a tiling or flags subtlety we can sample around — registration itself fails, so
there is no `CUarray` to read.

**This contradicts spec §8.2.** §8.2 rejected the external-memory route
partly *because* it "would therefore require an extra per-frame GPU copy",
implying graphics interop would not. Both routes need the copy. See "Spec
consequences" below.

### 2. Registration after `CopyResource` SUCCEEDS

Creating our own `ID3D11Texture2D` with `D3D11_RESOURCE_MISC_SHARED` +
`D3D11_BIND_SHADER_RESOURCE`, `CopyResource`-ing the WGC texture into it, and
registering *that* works. The probe implements both paths and reports which one
took effect, so this was measured rather than assumed.

Because the staging texture is **ours and persistent**, it is registered exactly
once for the whole session.

### 3. The NV12 conversion is visually correct

`frame.png` shows the desktop with correct geometry — no mosaic, no shuffled
tiles — correct colours (wallpaper, icon hues, taskbar), legible text, and the
captured cursor. Against the plan's decision table this is the
"correct desktop image → proceed" row. Reading through a `CUtexObject` with
`CU_TR_FILTER_MODE_POINT` and unnormalized coordinates handles the tiled layout
correctly, and the BT.709 limited-range coefficients with BGRA channel order
(`.x=B .y=G .z=R`) are right.

`frame.png` is not committed (see `.gitignore`); regenerate it as below.

### 4. Per-frame cost

200 iterations of the full per-frame GPU sequence — `CopyResource`, map,
`GetMappedArray`, `cuTexObjectCreate`, kernel launch, `cuTexObjectDestroy`,
unmap — with no host readback:

| | cold | p50 | p95 | p99 | min | max |
|---|---|---|---|---|---|---|
| 1024×768, cudarc 0.19 + NVRTC | 0.506 ms | **0.419 ms** | 0.469 ms | 0.506 ms | 0.380 ms | 0.546 ms |
| 1024×768, cudarc 0.16 + build-time PTX | 0.995 ms | **0.406 ms** | 0.479 ms | 0.494 ms | 0.378 ms | 0.995 ms |
| **1920×1080**, cudarc 0.16 + build-time PTX (**final**) | 0.478 ms | **0.430 ms** | 0.487 ms | 0.514 ms | 0.407 ms | 0.539 ms |

The first two rows agree within noise, confirming the cudarc version change
below costs nothing. The third is the shipping configuration at the target
resolution.

**§12's `<1 ms` budget holds at 1080p.** An earlier draft of this file predicted
otherwise — reasoning that 1080p is 2.6× the pixels of 1024×768 and so the p50
would land near 1.1 ms. **That prediction was wrong.** Measured, 1080p costs
0.430 ms versus 0.406 ms, a 6% increase for 2.6× the pixels. At these sizes the
sequence is dominated by fixed per-call overhead — map, `GetMappedArray`,
`cuTexObjectCreate`/`Destroy`, launch, unmap — not by pixel throughput. Useful
for Task 12: the per-frame cost is roughly resolution-independent up to 1080p, so
4K is likely fine too, but do not extrapolate the *other* way and assume a
cheaper path at lower resolutions.

The single cold pass is 0.506 ms, so there is no large one-off warmup in this
sequence — the 626 ms figure from an earlier run of this spike was CUDA context
creation plus PTX JIT plus host readback, none of which is per-frame work.

### 5. Texture objects cannot be cached across frames

The mapped `CUarray` is only valid between `cuGraphicsMapResources` and
`cuGraphicsUnmapResources`, so `cuTexObjectCreate` must be called per frame. It
is cheap enough to be inside the 0.419 ms above.

## Spec consequences

| Spec item | Change |
|---|---|
| §8.2 | The claim that graphics interop avoids a per-frame copy is **false**. A `CopyResource` is required. |
| §8.3 req. 1 | "Cache registrations, key a map on the texture pointer" is **obsolete**. WGC textures are never registered; our one staging texture is registered once. Task 12 needs no `TextureRegistry`. |
| D6 | Still the right call — graphics interop reaches a `CUarray`, which external memory cannot — but no longer for the "avoids a copy" reason. |
| risk #6 | Resolved, opposite to the assumption. WGC textures are unusable for direct registration; this **weakens** D6's rationale rather than strengthening it. |
| §12 | CUDA BGRA→NV12 measured at 0.419 ms p50 @1024×768; may exceed the `<1 ms` budget at 1080p. |

## Reproducing

The probe **must run in an interactive desktop session**. Over SSH, Windows puts
you in session 0 (services), where WGC fails with `ItemConvertFailed` before
capturing anything. Use the launcher:

```powershell
cd C:\path\to\pingpong\spikes\cuda-interop
cargo build --release
powershell -ExecutionPolicy Bypass -File ..\run-interactive.ps1 `
    -WorkDir $PWD -Exe .\target\release\cuda-interop.exe
```

Then open `frame.png` and look at it — that is the actual test.

## Notes for Task 12

- **`#[link(name = "nvcuda")]` does not work on MSVC.** There is no
  `nvcuda.lib`; the import library for `nvcuda.dll` is `cuda.lib`. Use
  `#[link(name = "cuda")]` plus a `build.rs` emitting
  `cargo:rustc-link-search=native=$CUDA_PATH\lib\x64` — without it the link
  fails with `LNK1181: cannot open input file 'cuda.lib'`. `build.rs` here is
  the working version.
- **`windows` must be 0.61, not the plan's 0.58**, to match
  `windows-capture` 1.5.0's `windows ^0.61.3`. Two `windows` majors in one build
  means two incompatible `ID3D11Texture2D` types.
- **`cudarc` must be 0.16, not the plan's 0.19.** `nvidia-video-codec-sdk` 0.4.0
  (latest) depends on `cudarc ^0.16.4`, and `Encoder::initialize_with_cuda`
  takes an `Arc<CudaContext>`. Task 12 has to share ONE CUDA context between
  this interop path and NVENC, and two cudarc majors would make those two
  incompatible types. **The whole project therefore pins cudarc 0.16.x.**
  Verified harmless: 0.16.6 exposes the same `CudaContext::new`,
  `default_stream`, `alloc_zeros`, `load_module`, `load_function`,
  `launch_builder` and `sys::cuTexObjectCreate` this spike uses, and re-running
  on it reproduced both the correct PNG and the same timings.
- **Do not use runtime NVRTC.** `cudarc::nvrtc::compile_ptx` on cudarc 0.16.6 +
  CUDA 12.6 aborts with
  `Expected symbol in library: GetProcAddress ... code 127` — its NVRTC sys
  layer expects symbols the installed `nvrtc64_*.dll` does not export. Compile
  PTX in `build.rs` with `nvcc --ptx` and load it with `Ptx::from_src`, which
  §8.3 requires anyway. `build.rs` here is the working version and Task 12
  should copy it.
- **`nvcc` needs `cl.exe`.** It shells out to the MSVC host compiler and fails
  with `Cannot find compiler 'cl.exe' in PATH` outside a Developer Command
  Prompt — which is the normal case over SSH. `build.rs` locates it via the `cc`
  crate and passes `-ccbin`.
- Resolved versions that linked and ran: `windows-capture` 1.5.0, `cudarc`
  0.16.6 with `features = ["cuda-12060", "driver"]` (no `nvrtc`), `windows`
  0.61, `image` 0.25, `cc` 1 as a build-dependency.
- `cudarc::driver::sys::cuTexObjectCreate` is exposed directly and dlopens the
  driver, so texture objects need no extra FFI of our own.
- `Frame::as_raw_texture()` gives `&ID3D11Texture2D`; `.as_raw()` from
  `windows::core::Interface` yields the `*mut c_void` the FFI wants.
- `cudarc`'s `memcpy_dtov` is deprecated in 0.19.8 in favour of `clone_dtoh`.

## Host display

The host came up at **1024×768 @ 60 Hz** on `\\.\DISPLAY1` (RTX 3070 Ti), single
display, with a `SudoMaker Virtual Display Adapter` also present but inactive —
the low mode is the usual fallback with no physical monitor attached.

It is now set to **1920×1080 @ 60 Hz** via `tools/set-resolution.ps1`, and the
final measurement above is at that resolution. Note the mode is not guaranteed to
survive a reboot or a monitor hotplug, so re-check it before Task 11's rate
measurement and Task 17's acceptance run:

```powershell
powershell -ExecutionPolicy Bypass -File tools\set-resolution.ps1 -ListOnly
```
