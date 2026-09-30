# Design history

pingpong was built in phases, each with a design written before the code and
measurements taken after it. These documents are that record. They explain
*why* the code is the way it is — most decisions here were measured, and the
measurements are kept — but they describe each phase as it was planned, so
parts of them have since been superseded. The current state of things is in
the [documentation](../README.md); code comments cite these documents by
section (`v1 design §5.1`, `v2 design §6.4`).

| Phase | What it set out to do | Documents |
|---|---|---|
| v1 (July 2026) | A video pipeline over the post-quantum tunnel: capture, NVENC, FEC, the wire format, VideoToolbox and Metal on the client | [v1-design.md](v1-design.md), [v1-measurements.md](v1-measurements.md) |
| v2 (August 2026) | Something to sit in front of: a virtual display at the client's mode, keyboard and mouse, the session lifecycle | [v2-design.md](v2-design.md), [v2-input-design.md](v2-input-design.md), [v2-measurements.md](v2-measurements.md) |
| v3 (September 2026) | Moonlight + Apollo parity: their video path, a host service with a web UI, pairing and discovery, an app | [v3-design.md](v3-design.md) |
| v4 (September 2026) | One client and one host, in Rust, on macOS, Windows and Linux; AI agents | [../architecture.md](../architecture.md), [../platforms/](../platforms/) |

Reading them:

- **[V]** marks a claim verified (against source code, a spec or a
  measurement), **[P]** one that was plausible but not yet verified.
- "Task N" and "Phase N" refer to the implementation plans of the time, which
  are in the git history only.
- Names of machines are generic ("the test host": a Windows 11 PC with an
  RTX 3070 Ti; the client: a 14" MacBook Pro).
- The [spikes](../../spikes/README.md) are the throwaway experiments these
  designs rested on.
