# Spikes

Throwaway experiments that settled the riskiest questions before the real
code was written, in the project's first phases (July and August 2026). Each
README records what was measured and what it decided; the code is kept only
so the measurements can be repeated.

| Spike | Question | Platform |
|---|---|---|
| [vt-latency](vt-latency/README.md) | Does VideoToolbox decode in real time, one frame in, one frame out? | macOS |
| [nvenc-min](nvenc-min/README.md) | The NVENC P1 ultra-low-latency setup, and its encode latency | Windows, NVIDIA |
| [cuda-interop](cuda-interop/README.md) | Getting a captured D3D11 texture to NVENC through CUDA | Windows, NVIDIA |
| [vdd-mode](vdd-mode/README.md) | Can a virtual display be put in any mode, and made primary? | Windows |

Some of what they decided has since been replaced (the host now converts
colour with its own D3D11 shaders and captures with Desktop Duplication;
see [docs/architecture.md](../docs/architecture.md)).

- The spikes are not workspace members: each has an empty `[workspace]`
  table and builds on its own (`cargo build --release` in its directory), so
  a broken probe never gates the workspace's tests.
- `§` references are to the design documents in
  [docs/design/](../docs/design/README.md); "Task N" and "risk #N" to the
  implementation plan of the time, which is in the git history only.
- Windows spikes that capture or change displays must run in the interactive
  desktop session, not over SSH (session 0): see
  [run-interactive.ps1](run-interactive.ps1).
