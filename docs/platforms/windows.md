# Windows

Pong on Windows is the host pingpong was built around: what Apollo is to
Moonlight. Ping also runs on Windows 10 and 11.

## Pong, the host

### What it needs

| | Why |
|---|---|
| Windows 10 or 11, 64-bit | |
| An NVIDIA GPU with NVENC, and its driver | Encoding. NVENC is loaded from the driver (`nvEncodeAPI64.dll`) at runtime: no CUDA toolkit or Video Codec SDK is needed to build or run |
| [SudoVDA](https://github.com/SudoMaker/SudoVDA) (SudoMaker Virtual Display Adapter) | A virtual display at each client's mode. Apollo installs it; it can also be installed on its own. Pong and Apollo cannot stream at the same time (they share the driver) |
| [ViGEmBus](https://github.com/nefarius/ViGEmBus) (optional) | Controllers: each of the client's pads becomes a virtual Xbox 360 pad |
| Steam's **Steam Streaming Speakers** (optional) | Sound only on the client: during a session the host's default output moves to this virtual device, so the host's speakers stay quiet (Apollo's approach). Without it, the sound plays on the host too |

### How it runs

`PongService` (LocalSystem, automatic start) launches `pong host` into the
active console session with a copy of its SYSTEM token, and relaunches it
when the console session changes (sign-out, fast user switching, a reboot to
the sign-in screen) or the host exits. Only SYSTEM can open the secure
desktop, so only a host running this way can capture and control UAC
prompts, the lock screen and the sign-in screen — the model Sunshine and
Apollo use (`pong/src/service.rs`).

State lives in `C:\ProgramData\Pong\`: `config.toml`, the host's identity,
paired clients, the web UI's certificate and accounts, and `logs\`. The host
makes that folder private when it starts (`pong/src/private.rs`): only
SYSTEM and Administrators can read it, apart from the two files Pong's
window needs (`config.toml`, `web-cert.pem`).
`tools/host-deploy.ps1` installs a build as the service and adds Pong's
window to the Start menu; see [../install.md](../install.md).

### A session, on Windows

- **The display.** A SudoVDA virtual display is added in the client's exact
  mode, made primary with the display-configuration (CCD) API, and — unless
  the client or the settings keep them — the host's own monitors are turned
  off, so the virtual display is the whole desktop and windows and games
  open where the client can see them (Apollo's default). A keepalive thread
  pings the driver every second: SudoVDA removes a monitor ~3 s after the
  last ping. At the end the virtual display goes first, then the host's
  arrangement comes back. `pingpong-display/src/windows.rs` explains each of
  these, and why the obvious alternatives fail.
- **Capture.** DXGI Desktop Duplication, re-attached to the input desktop
  whenever duplication is lost, so the secure desktop is captured too. The
  process's GPU scheduling priority is raised (as Apollo does) so a game
  saturating the GPU does not starve capture and encode.
- **Colour.** BGRA → NV12 in the host's own D3D11 shaders: BT.709, limited
  range, chroma sited left, the colour description written into the stream.
- **Encode.** NVENC P1, ultra-low-latency tuning, CBR with a single-frame
  VBV, two-pass at quarter resolution (a setting, on by default), an
  infinite GOP and five reference frames where the GPU supports them, so a
  lost frame is repaired by invalidating it as a reference rather than by a
  keyframe.
- **Sound.** WASAPI loopback of the session's virtual sink, stereo to 7.1,
  Opus as Sunshine configures it.
- **Input.** `SendInput`: keys by scancode, the pointer absolute over the
  virtual desktop or relative for games. Typed text (an agent's `type`, a
  pasted clipboard) goes one character every 15 ms: Windows 11's WinUI text
  fields garble faster Unicode keystrokes.
- **The pointer.** The foreground application's pointer shape and state are
  sent to the client, which draws the pointer itself (no round trip of lag)
  and switches between desktop and game mode on its own. The global cursor
  is read first; the foreground thread's input queue is joined only when
  that says nothing, and never near a click, because joining resets the
  queue's double-click state.
- **Controllers.** ViGEm pads, one per client pad, with rumble sent back.
- **Clipboard.** A helper runs as the signed-in user for the session
  (`pong clipboard-agent`): SYSTEM sees only part of the user's clipboard,
  and must not read files on the user's behalf.
- **Awake.** The display and system are kept awake for the session: Windows
  powering the display off at its idle timeout would stop the capture.

### Starting a stream

From the tunnel up to the session started, 3024x1890@120:

| The host's own monitor | Virtual display on the desktop | Tunnel → session started |
|---|---|---|
| Awake | 153–181 ms | 0.87–1.5 s |
| Switched off at its button (still connected) | ~3.9 s | 5.2–6.0 s |
| Unplugged (headless) | 327–372 ms | 0.62–0.79 s |

An asleep or switched-off monitor is still on the cable, and Windows spends
~3.7 s on it whenever the display arrangement changes. Apollo pays the same.

### Limits and known issues

- **Network priority (QoS).** Sunshine marks its traffic for Wi-Fi's video
  queue with qWAVE; qWAVE will not take Pong's dual-stack socket. It would
  take an IPv4 socket of its own, or a system QoS policy on the host.
- **Ending with the monitor asleep.** Bringing a DisplayPort monitor back
  from deep sleep blocks the display change for as long as it takes to wake
  (up to ~26 s measured); a client connecting meanwhile waits.
- **AV1** needs an NVIDIA GPU that encodes it (RTX 40 series and later).
- HDR and YUV 4:4:4 are not built.

## Ping, the client

As Moonlight does it on Windows:

- **Decode.** FFmpeg's decoders with the D3D11VA hwaccel, on the same
  Direct3D 11 device that presents, on a thread of their own. Each picture
  is copied out of the decoder's texture array into one of five textures of
  Ping's at once, so the decoder's pool is never held by the screen.
- **Present.** A flip-model swap chain with frame latency one: the newest
  frame, never a queue. Full screen is a borderless window over the monitor,
  which Windows flips straight to the display. Frame pacing draws one frame
  per refresh on DXGI's vblank.
- **Keyboard and mouse.** While captured, a low-level keyboard hook takes
  every key — the Windows key and Alt+Tab included, full screen — as SDL's
  keyboard grab does for Moonlight. On the desktop the pointer is Windows'
  own in the host application's shape (the hardware cursor: no lag),
  confined to the window; when a game takes the mouse, raw input's relative
  motion goes instead.
- **Controllers.** XInput, slot for slot; the Guide button through
  `XInputGetStateEx`; rumble through `XInputSetState`.
- **Sound.** WASAPI shared mode on the default device, following it when it
  changes.
- **Timers.** 1 ms timer resolution while streaming.

Building it needs an FFmpeg 8 shared build and LLVM (for the FFmpeg
bindings): see [../install.md](../install.md#ping-on-windows).

Not verified yet: keyboard, mouse and controllers streaming another machine
(tests so far streamed the same PC, which feeds input back into itself), and
on-glass timing on a real monitor.
