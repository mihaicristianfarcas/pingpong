# macOS

Ping and Pong both run on macOS 14 or later (Apple silicon and Intel; the
measurements here are from an M4 Pro).

## Ping, the client

- **Window.** A stream opens in a window of its own. Full screen (the
  default) it goes into a Space of its own, as Moonlight's does: Ping's own
  window stays on the desktop a swipe away, and the Dock and menu bar stay
  hidden in the stream's Space. Ctrl+Option+Shift+X switches between full
  screen and a window. If macOS refuses full screen (a stream started just
  as another left its Space), Ping asks again until it gets it.
- **Picture.** VideoToolbox decodes in real time — one frame in, one frame
  out, no buffering — and the pixel buffer is bound to Metal without a copy
  (`CVMetalTextureCache`). Without frame pacing the newest frame is drawn at
  once; with it, Moonlight's pacer draws one frame per display refresh on a
  `CVDisplayLink` tick.
- **Recording the screen, or something over the stream.** Full screen with
  nothing over it, macOS takes the picture straight to the display, and one
  frame at a time waits for it, so the next frame drawn is the next shown.
  A screen recording, a window, menu or notification over the stream, a
  window rather than full screen, or HDR makes macOS composite the picture
  first, two to three refreshes later. Ping tells which from how long its
  frames take to reach the screen, lets three wait while it lasts, and
  goes back to one after; every frame is still shown (the log says "the
  stream is composited"). The composited path costs latency: about 22 ms
  from decode to the screen at 60 fps, against 6 ms (see
  [benchmarks](../benchmarks.md#recording-the-screen-macos-client)).
  macOS also takes a recorded stream straight to the display at times
  and shows some of its frames a refresh late; at the display's full rate
  (120 fps at 120 Hz), Ping then shows 105–117 frames of 120 rather than
  add a refresh to every one.
- **Low Power Mode.** On a MacBook Pro it holds the 120 Hz panel at
  60 Hz, and at 60 Hz a frame reaches the screen two refreshes after it
  is drawn with V-Sync on: 33–39 ms from decode to the screen, against
  6 ms with Low Power Mode off (an Xbox stream's 60 fps, measured on an M4
  Pro; [benchmarks](../benchmarks.md#low-power-mode-and-60-hz-macos-client)).
  While it is on, the stream says so in its corner (with **Connection
  warnings** on, Settings > Video) and the log says when it changes. Turn
  it off (System Settings > Battery), or turn V-Sync off, which tears but
  measured 3.6 ms. A 60 Hz external display costs the same with V-Sync on.
- **AV1.** On a Mac with an AV1 decoder (M3 and later), VideoToolbox
  decodes it in hardware, 8- and 10-bit: the format description carries an
  `av1C` record built from the stream's sequence header, and samples go in
  without temporal delimiters, as MP4 stores them. Settings > Video offers
  AV1 only there.
- **HDR and 4:4:4.** On Apple silicon, VideoToolbox decodes HEVC Main10
  and HEVC 4:4:4 (8- and 10-bit) in hardware (measured on an M4 Pro); Ping
  asks for 10-bit pictures as `x420`/`x444`, which Metal samples plane by
  plane (VideoToolbox's own are packed). With **HDR** on (Settings >
  Video) and a display with headroom above SDR white (an XDR panel, an
  HDR monitor), the stream's layer is BT.2100 PQ, 10 bits a channel, with
  the host's HDR metadata (`CAEDRMetadata`), as Moonlight's
  `vt_metal.mm` does: the host's SDR white lands on this Mac's, and
  brighter goes into the display's headroom. Text and statistics drawn
  over an HDR picture are converted to it.
- **Keyboard and mouse.** While captured, the pointer is locked and hidden,
  and its relative motion goes to the host (or its position, after
  Ctrl+Option+Shift+M); the pointer you see is the host's, in the picture.
  Pressing ⌘⇧ gives the pointer to the Mac, shown and free, so a
  screenshot shortcut's crosshair can show and move; a key that reaches the
  stream with them was no system shortcut and gives the pointer back. The
  tool usually takes the release of ⌘⇧, so the stream reads the keyboard's
  state on its next event, and takes the pointer back once no other window
  is under it. Command can be forwarded as the Windows key
  (**Settings > Input**); against a Mac host it always is.
- **Controllers.** GameController, with the Xbox button read from the pad's
  HID reports, as Moonlight does; rumble through the pad's haptics. macOS
  26 also opens its Games overlay on that button, as it does with Moonlight.
- **Sound.** CoreAudio, stereo to 7.1.
- **Local network.** macOS asks once whether Ping may use the local network
  (it declares `NSLocalNetworkUsageDescription`); without it, discovery and
  LAN connections fail.
- **Notifications** for agent sessions (when a session needs you and is not
  on screen) are asked for the first time a session opens.
- **The menu bar** is a Mac app's: Ping (About, Check for Updates,
  Settings, Services, Hide, Quit), File (Add Host, Look for Hosts Again,
  Close Window), Edit, View (the pages), Window and Help (the docs, the
  logs in Finder, reporting an issue).
- **One copy.** Opening Ping again brings the running one forward.

## Pong, the host

Pong also runs on a Mac, with the Windows host's session rules and the same
wire protocol. A client cannot tell them apart, except that the session ack
says which system the host runs (so Ping forwards Command as Command).

| | Windows host | macOS host |
|---|---|---|
| Display for the session | SudoVDA virtual display, primary, others off | `CGVirtualDisplay` (private CoreGraphics) in the stream's mode (HiDPI from 2560 px wide), main, the Mac's own displays mirroring it |
| Capture | Desktop Duplication | ScreenCaptureKit, NV12 video range, BT.709; for HDR its HDR10 capture (`x420`, BT.2100 PQ) |
| Colour conversion | D3D11 shaders | none (ScreenCaptureKit delivers NV12) |
| Encoder | NVENC, reference invalidation | VideoToolbox hardware HEVC/H.264 (HEVC Main10 for HDR), low-latency rate control, recovery from long-term references |
| Sound | WASAPI loopback of a virtual sink, up to 7.1 | ScreenCaptureKit's system mix in stereo; up to 7.1 from a Core Audio tap of a surround output |
| Input | `SendInput` | CoreGraphics events at the HID tap |
| Pointer | Desktop Duplication's, drawn into the picture | ScreenCaptureKit draws it into the picture |
| Keeping awake | `SetThreadExecutionState` | IOKit power assertions (an asleep display cannot be captured) |

Everything after the encoder is shared with the other hosts: the paced
sender, FEC, adaptive bitrate, the tunnel, pairing, discovery, reconnection
and the web UI. The Mac-specific modules are in `pong/src/mac/`.

### Pong's menu bar icon

Pong Control.app, the window, is also Pong's menu bar icon: a menu bar item
without a Dock icon or a place in the app switcher (`LSUIElement`, and the
accessory activation policy once GPUI has launched it), as menu bar apps
are. Its menu says what the host does, names a device asking to pair (a
notification says so too), opens the window, and says when there is an
update. It starts at login with a LaunchAgent of its own
(`dev.pingpong.PongControl`), without its window; **Show Pong's icon at
login** in General turns that agent on and off. Quitting it leaves the host
running: the host is Pong.app, under a LaunchAgent that keeps it alive.

When Pong.app is beside it and the host is not running, the window's
**Start Pong** writes the host's LaunchAgent and loads it: that is how a
copy installed with Homebrew, where no install script runs, starts the host.

### Permissions

Pong needs **Screen Recording** (to capture) and **Accessibility** (to
inject the keyboard and mouse). As an app (`tools/build-pong-app`) it asks
for both in its own name at start. From a terminal, the terminal needs them.
Without Accessibility, Pong tells the client, and Ping shows that the host
is ignoring its keyboard and mouse. Sound in surround needs **System Audio
Recording** besides (below): Pong asks for it the first time a client wants
5.1 or 7.1, and that session streams stereo while you answer.

macOS ties these permissions to the app's signature. An ad hoc signature
changes with every build, so each rebuild would lose them;
`tools/dev-signing-identity` creates one self-signed identity that the
build scripts sign with, so permissions survive rebuilds. Releases are
signed with the project's Developer ID, the same from one release to the
next, and notarized.

### Sound in surround

ScreenCaptureKit, which Pong captures the screen with, mixes the system's
sound down to stereo whatever it is asked for (2 channels at 48 kHz when
asked for 6 or 8, measured on macOS 26.5). A client that asks for 5.1 or 7.1
gets it from a Core Audio tap instead (macOS 14.2 and later), which hears
what apps play to an output device in that device's own channels. Apps play
surround only to an output that has the speakers for it, so the Mac needs
one:

- **A receiver or TV with surround on HDMI**, set to 5.1 or 7.1 in Audio
  MIDI Setup (**Configure Speakers**). Pong taps it while it stays the
  default output, and keeps the sound off it unless the client asks for the
  sound on the host too (`--host-audio`), as the Windows host does.
- **A loopback device**, as Sunshine uses on a Mac: install
  [BlackHole](https://github.com/ExistentialAudio/BlackHole) 16ch
  (`brew install --cask blackhole-16ch`). For each surround session Pong
  makes it the default output, sets its speakers to the session's 5.1 or
  7.1, and puts back the previous output and speakers when the session
  ends, as the Windows host does with Steam Streaming Speakers. The Mac
  plays nothing aloud meanwhile; with `--host-audio` Pong leaves the output
  alone and the session is stereo.

The channels go into the stream in Moonlight's order whatever order the
device has them in, and a device that does not run at 48 kHz is resampled
(set it to 48 kHz in Audio MIDI Setup for the best sound). Measured with
BlackHole 16ch, streaming the Mac to itself: a test file sounding one
channel at a time came out of the client's decoder in the same channel,
for 5.1 and for 7.1, each at its level (a 5.1 file's back pair 3 dB lower:
macOS plays its rear speakers into the 5.1 device's surround pair). With
neither, or without the permission, sessions are stereo and Pong's log
says why.
`cargo run -p pingpong-audio --example mac-outputs` lists the outputs and
what each can carry; `-- --tap 5` also listens to the default one for five
seconds and prints each channel's level.

### Measured (M4 Pro, client and host on one Mac)

- Session up **238 ms** after the tunnel (a Windows host takes seconds,
  mostly Windows bringing the virtual display up); the virtual display
  plugged in and made main in ~100 ms.
- VideoToolbox HEVC encode costs **~2.7 ms per megapixel**: 6.9 ms at 1080p,
  15–16 ms at 3024x1890 — so that size tops out near 55 fps with one frame
  in the encoder. Two in flight gave 63 fps but 27 ms from capture to
  encoded; latency wins. H.264 cannot do 3024x1890 at 120 at all (past its
  level limits, the encoder drops every frame).
- Loss: 5% in bursts of 3 recovered by long-term-reference refreshes, with
  no keyframes and no decode errors.
- Sound: 200 packets a second, nothing concealed.
- Pong killed mid-stream and restarted: Ping renegotiated, and video was
  back 0.4 s after Pong was.

### Behaviour worth knowing

- **One virtual panel.** macOS holds a display it has never seen offline
  until someone answers "What do you want to show on Pong?". So the virtual
  panel is the same every session (3840x2400) and put in the session's mode
  once online: sizes from 800x600 to 3024x1890 come up in 0.1–0.7 s without
  a prompt. If it does not come up within 2 s, Pong streams the main display.
- **Lid closed, no display**: fine — the session's virtual display is what
  gets captured. A locked Mac streams its lock screen.
- **Loss recovery** uses VideoToolbox's long-term references. The protocol
  has no per-frame acknowledgement, so Pong counts a frame as arrived once
  200 ms pass with no loss reported for it, and acknowledges it to the
  encoder; after a loss the next frame refreshes from an acknowledged one. A
  wrong guess costs a keyframe, as before.
- **The pointer** is in the picture: ScreenCaptureKit draws it, in its
  real shape, and hides it when an app does.
- **HDR** (macOS 15 or later, Apple silicon): for a client that asks, the
  virtual display is an HDR one (a `CGVirtualDisplayMode` with a transfer
  function, measured to give the screen 5x SDR white of headroom),
  captured with ScreenCaptureKit's HDR10 preset and encoded as HEVC Main10
  with BT.2020 primaries, the PQ curve and HDR10 metadata. Verified Mac to
  Mac on one M4 Pro: 52-53 fps of desktop at 1920x1200, no loss.

### Limits

- No 4:4:4: VideoToolbox encodes HEVC at 4:2:2 at most (its RExt profile
  is 4:2:2 10-bit).
- Sound is stereo unless the Mac has a surround output and Pong may record
  the system's sound (see [Sound in surround](#sound-in-surround)).
- No controllers yet: macOS makes virtual controllers only for a program
  signed with the HID Virtual Device entitlement
  (`com.apple.developer.hid.virtual.device`), which Apple grants on
  request.
- Streaming a Mac to itself (a loopback test) reports no presented frames:
  the client's window sits on the display being streamed, which reports no
  presentation times.
