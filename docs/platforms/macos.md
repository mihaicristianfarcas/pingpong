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
- **Keyboard and mouse.** While captured, the pointer is locked and hidden
  (relative motion for games) or free and drawn by Ping in the host's
  pointer shape (the desktop). Command can be forwarded as the Windows key
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

## Pong, the host

Pong also runs on a Mac, with the Windows host's session rules and the same
wire protocol. A client cannot tell them apart, except that the session ack
says which system the host runs (so Ping forwards Command as Command).

| | Windows host | macOS host |
|---|---|---|
| Display for the session | SudoVDA virtual display, primary, others off | `CGVirtualDisplay` (private CoreGraphics) in the stream's mode (HiDPI from 2560 px wide), main, the Mac's own displays mirroring it |
| Capture | Desktop Duplication | ScreenCaptureKit, NV12 video range, BT.709 |
| Colour conversion | D3D11 shaders | none (ScreenCaptureKit delivers NV12) |
| Encoder | NVENC, reference invalidation | VideoToolbox hardware HEVC/H.264, low-latency rate control, recovery from long-term references |
| Sound | WASAPI loopback of a virtual sink, up to 7.1 | ScreenCaptureKit's system mix, stereo |
| Input | `SendInput` | CoreGraphics events at the HID tap |
| Pointer | drawn by the client from cursor state | drawn by the client, the shape matched against the standard cursors |
| Keeping awake | `SetThreadExecutionState` | IOKit power assertions (an asleep display cannot be captured) |

Everything after the encoder is shared with the other hosts: the paced
sender, FEC, adaptive bitrate, the tunnel, pairing, discovery, reconnection
and the web UI. The Mac-specific modules are in `pong/src/mac/`.

### Permissions

Pong needs **Screen Recording** (to capture) and **Accessibility** (to
inject the keyboard and mouse). As an app (`tools/build-pong-app`) it asks
for both in its own name at start. From a terminal, the terminal needs them.
Without Accessibility, Pong tells the client, and Ping shows that the host
is ignoring its keyboard and mouse.

macOS ties these permissions to the app's signature. An ad hoc signature
changes with every build, so each rebuild would lose them;
`tools/dev-signing-identity` creates one self-signed identity that the
build scripts sign with, so permissions survive rebuilds.

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
- **The pointer** is read 20 times a second (`NSCursor.currentSystemCursor`,
  matched against the standard cursors by hot spot, size and image count)
  and drawn by the client. An app's own cursor is drawn as the arrow. The
  pointer is always reported visible (a Mac hides it while you type, which
  must not flip the client into game mode); Ctrl+Option+Shift+M switches the
  mouse mode for a game that takes the mouse.
  `cargo run -p pingpong-input --example mac-cursor-probe` shows what the
  watcher reads.

### Limits

- Sound is stereo: ScreenCaptureKit's system mix.
- No controllers: macOS has no virtual gamepad API short of a DriverKit
  driver.
- Streaming a Mac to itself (a loopback test) reports no presented frames:
  the client's window sits on the display being streamed, which reports no
  presentation times.
