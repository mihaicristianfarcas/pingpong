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

### Pong's icon in the notification area

Pong's window (`Pong Control.exe`) is also Pong's icon in the taskbar's
notification area, as Apollo's is: it runs as the signed-in user, not as
the service, so what its menu opens opens on that user's desktop. A click
on the icon opens the window; the right button has the menu (what the host
does, a device asking to pair, the window, an update, **Quit Pong
Control**). A device asking to pair is also a Windows notification. The
icon starts at sign-in from the user's `Run` key
(`"C:\Program Files\Pong\Pong Control.exe" --background`), which
`host-deploy.ps1` sets and **Show Pong's icon at login** turns on and off.
If Explorer restarts, the icon comes back with it (the app listens for
`TaskbarCreated`). Starting the window again while it runs opens the running
one's window.

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
- **Input.** `SendInput`: keys by scancode, the mouse relative (the host's
  pointer speed applies, as with Apollo) or as positions over the display,
  mapped from the desktop as it is at each move, so a game that changes the
  display's resolution does not move every click. Typed text (an agent's
  `type`, a pasted clipboard) goes one character every 15 ms: Windows 11's
  WinUI text fields garble faster Unicode keystrokes.
- **The pointer.** Desktop Duplication reports the pointer beside the
  desktop image, never in it: its place, whether it shows, and its shape.
  Pong draws it into a copy of the desktop on the GPU, as Apollo does, so
  the client sees the pointer the host's screen shows, hidden whenever a
  game hides it. A pointer that moves over a still desktop makes a new
  frame.
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
- **Keeping the host's monitors on, with its monitor asleep or switched
  off.** With **Keep this PC's monitors on while streaming** (or
  `ping stream --keep-host-displays`) the host's own monitor stays part of
  the desktop, so Windows brings it up before the session starts: 12 to 38 s
  measured with the monitor switched off at its button, against about a
  second with it awake. The client waits up to 45 s for a session to start.
  The default mode does not pay this: it takes the host's monitors out of the
  desktop.
- **Ending with the monitor asleep.** Bringing a DisplayPort monitor back
  from deep sleep blocks the display change for as long as it takes to wake
  (up to ~26 s measured); a client connecting meanwhile waits.
- **AV1** needs an NVIDIA GPU that encodes it (RTX 40 series and later).
- HDR and YUV 4:4:4 are not built.
- **Not code-signed.** Pong and Ping for Windows are not signed:
  SmartScreen asks before each first start, and Smart App Control, where
  it is on, blocks them ([../install.md](../install.md#on-windows)).

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
  keyboard grab does for Moonlight, except the screenshot shortcuts (Print
  Screen, Win+Shift+S, Win+Shift+R): those are handed back to Windows
  whole, sent with `SendInput` and marked so the hook lets them through.
  The pointer is confined to the window
  and hidden (the host draws its own into the picture); raw input's
  relative motion goes to the host, or the pointer's position after
  Ctrl+Alt+Shift+M.
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
