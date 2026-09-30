# Linux

Ping and Pong both run on Linux, under X11 and Wayland. Linux support is the
youngest: it is developed and tested in a container (Ubuntu 24.04, Xvfb,
Mesa's software renderer), and has not yet been tried on much real hardware
(see "Not verified yet" below).

## Ping, the client

The same app as on the other systems. Because winit allows one event loop
per process and the app's window has it, a stream runs in a second process
of the app (`ping-app --ping-stream`), which tells the app how it ended on a
line of its standard output. That process is `ping_core::linux`:

- **Window and input.** A winit window (X11 or Wayland). Keys go as
  scancodes through the shared keyboard handling (Moonlight's
  Ctrl+Alt+Shift chords); the pointer is captured by locking or confining
  the cursor.
- **Decode.** FFmpeg, with VA-API where the machine has it, else four slice
  threads in software. `PING_SOFTWARE_DECODE=1` skips VA-API.
- **Present.** wgpu (Vulkan or GL), drawing the planes (NV12, I420 or I444)
  with the same colour maths as the Windows renderer.
- **Controllers.** gilrs (evdev), rumble included.
- **Sound.** cpal (ALSA; PipeWire and PulseAudio through it).

## Pong, the host

The same host — sessions, pairing, the web UI, the bitrate controller, the
internet path — with `pong/src/linux/` where Windows and macOS have theirs.
It streams the screen as it is: there is no virtual display, so the picture
is scaled to the client's size (letterboxed when the shapes differ, the
pointer mapped to match).

### Under X11

- **Capture.** MIT-SHM into memory shared with the X server, XDamage to
  know when the screen changed, the pointer drawn in from XFixes
  (`pingpong-capture/src/x11.rs`; x11rb, no C libraries).
- **Encode.** FFmpeg, trying VA-API (Intel, AMD), then NVENC, then x264
  (H.264 only, in software), set up as Sunshine sets them up: no B-frames,
  an endless GOP, a VBV of about a frame, an IDR on a loss (none of these
  encoders invalidates references). swscale converts to BT.709 4:2:0 and
  scales (`pingpong-encode/src/ffmpeg.rs`).
- **Input.** XTest: keys by position, text by the layout's own keys (spare
  key codes remapped only for characters the layout lacks), the wheel as
  buttons 4–7 (`pingpong-input/src/x11.rs`).
- **Sound.** The default output's monitor through libpulse-simple, which
  PipeWire serves too; stereo.
- The screensaver and DPMS are held off during a session.

### Under Wayland

The screen and the input go through the desktop portal, the way desktop
sharing is meant to work there (`pong/src/linux/portal.rs`): a RemoteDesktop
session with a screen cast in it (GNOME, KDE), or a ScreenCast session alone
where the desktop has no RemoteDesktop portal (wlroots: sway, Hyprland — no
input from the client there). Frames come over PipeWire with the pointer
drawn in by the compositor; keys go as Linux key codes, text as keysyms,
the pointer as the portal's motion and buttons.

The first time, the desktop asks whoever is at the host — Pong asks as it
starts, not when a client connects. On GNOME, switch on **Allow Remote
Interaction** in that dialog for the keyboard and mouse. The portal then
gives a restore token (kept as `portal-token` in Pong's data folder), and
later sessions start without asking. The screen is kept on through the
Inhibit portal. `PONG_CAPTURE=x11|portal` overrides the choice where the
environment does not make it clear.

### Running it

Pong runs in the desktop session, as the user: `tools/linux/pong.service` is
a systemd user unit for that (installation in [../install.md](../install.md#pong-on-linux)).
Data and logs are in `~/.config/pong`.

## Measured (in the container)

| What | Result |
|---|---|
| Linux host to Linux client, 1920x1080 screen to a 1280x720@60 stream, x264 in software | 60 fps; 4.2 ms capture to encoded; client decode 0.3 ms; 8.5 ms capture to screen; clicks, keys, typed text ("Hé!") and the wheel arrive; a 440 Hz tone at 200 packets a second |
| The same through the Wayland portal (GNOME 46, nested, software rendering) | frames at the 42 fps the nested compositor draws; 4.7 ms capture to encoded; 10–11 ms capture to screen; input arrives; later sessions start from the restore token without asking |
| Linux client streaming a Windows host over a VPN, 1280x720@60 | H.264 and HEVC; 60 fps received, decoded and presented; 1 ms decode (software); 14–20 ms capture to screen |
| Offscreen decode and present of the 600-frame test stream (`ping-core/examples/linux-render-check.rs`) | every colour bar matches FFmpeg's own BT.709 conversion exactly |

`tools/linux/loopback-test` repeats the first row; see
[../development.md](../development.md#linux).

## Not verified yet

- Real hardware: VA-API decode and encode, NVENC, controllers, a real
  window manager, compositor and cursor theme, keyboard and mouse through
  the client's window (the container has none; scripted input goes around
  the window).
- The wheel under Wayland: the nested test desktop gives X clients no wheel
  events at all, so the portal's scroll is sent but unseen.
- KDE and wlroots desktops (Ubuntu 24.04's wlroots portal needs a GPU to
  share the screen, and the container has none).

## Not built yet

- Input on wlroots desktops (no RemoteDesktop portal there): uinput, or
  their virtual keyboard and pointer protocols.
- GNOME without the dialog, and at the client's resolution: mutter's own
  screen-cast API can record a virtual monitor of any size (what
  gnome-remote-desktop does).
- The host at the client's resolution under X11 (RandR modes) rather than
  scaled.
- Controllers on a Linux host (uinput gamepads).
- Packages: there is a systemd unit, but no `.deb` or install script.
