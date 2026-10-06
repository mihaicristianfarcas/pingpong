# Xbox

Ping streams from an Xbox: one of your consoles (an Xbox One or Series X|S,
anywhere it can be reached), or a game in Xbox Cloud Gaming. It uses the same
window, hardware decoder, presenter, speakers and controllers as a stream
from Pong, and nothing on the console changes. Ping speaks the protocol
Microsoft's own web client speaks, as
[Greenlight](https://github.com/unknownskl/greenlight) does.

## What you need

- A **Microsoft account** with an Xbox profile (one that has signed in to an
  Xbox, or at xbox.com).
- For **a console**: the same account signed in on it, and on the console
  *Settings > Devices & connections > Remote features > Enable remote
  features*. Set *Settings > General > Power options* to **Sleep**, so Ping
  can wake it; a console in energy-saving mode has to be turned on by hand.
  On another network than the console, its router has to let the stream
  through (UPnP, as the Xbox app needs too).
- For **the cloud**: Game Pass Ultimate for its catalogue, or any account
  for the free-to-play games, in a country where Xbox Cloud Gaming is
  offered.

## Using it

1. In Ping's sidebar, **Xbox**, then **Sign In**. Ping shows a code: open
   microsoft.com/link on any device (**Open the Page** does it here), enter
   the code, and sign in there. Ping never sees your password; the sheet
   closes when you are done.
2. The page lists your consoles, asleep or on, the cloud games your
   account may play, and which of your friends are online and what they
   play. **Stream** starts a console's stream, waking it if it
   sleeps; **Play** starts a cloud game, through the cloud's queue when
   there is one. **Turn On** and **Turn Off** wake a console or turn it off.
3. Ctrl+Alt+Shift+Q (⌃⌥⇧Q) ends the stream, as for a Pong host. The console
   is free again at once.

The stream takes the size and display settings from Ping's settings; the
console chooses its own codec (H.264), frame rate and bitrate, and sends up
to 1080p. The statistics (Ctrl+Alt+Shift+S) work as for Pong.

**Controllers**: up to four, as the console's first to fourth controllers,
with rumble when the console sends it. The Xbox button is the Guide button.

**The keyboard** is the first controller by default, so the console's own
interface and every game can be played without one
(*Settings > Input > Keyboard as an Xbox controller*):

| Key | Controller |
|---|---|
| Enter, Space | A |
| Backspace, Esc | B |
| X, Y | X, Y |
| Arrows | D-pad |
| `[` `]` | Left and right bumper |
| `-` `=` | Left and right trigger |
| L, R | Left and right stick click |
| M, V | Menu, View |
| N | The Xbox button |

Keys are positions: on any keyboard layout they are where the US layout has
them. Turned off, the console gets a keyboard and a mouse instead, for games
that take them (on Xbox Cloud Gaming, games marked "mouse and keyboard").

From the command line:

```sh
ping xbox sign-in
ping xbox consoles
ping xbox stream "Living room"
ping xbox games
ping xbox play "Fortnite"
```

([cli.md](cli.md#ping-xbox) has all of them.)

## Against Greenlight

| | Greenlight | Ping |
|---|---|---|
| Sign-in | Device code, Microsoft account | The same, in a sheet with the code; tokens renewed and kept between runs |
| Consoles | List, power on and off | The same; a sleeping console is woken before its stream, with "Waking…" said |
| Cloud | Library, recent, queue | Library with search, queue with its estimated wait; free-to-play for an account without Game Pass |
| Video decode | Chromium's WebRTC pipeline and jitter buffer, then a `<video>` element or WebGPU | Each frame to the platform's hardware decoder the moment its last packet arrives, presented as Pong's are (Metal, Direct3D 11, wgpu) |
| Loss | Chromium decodes on through the gap | Moonlight's rule: nothing decoded until a keyframe, asked for at once both ways (RTCP PLI and the console's keyframe request) |
| Picture size | Says 1920×1080 to the console whatever the window | Says the size Ping shows it at |
| Away from home | Teredo addresses turned into IPv4 candidates | The same, and Ping's own public address (STUN) offered, which Greenlight leaves to do |
| Controllers | The browser's Gamepad API; rumble on xCloud | The platform's own (GameController, XInput, evdev), with rumble wherever the console sends it; four controllers announced as they come and go |
| Keyboard | As a controller, or a keyboard | The same, by key position; a button two keys hold stays down until both are up |
| Audio | Chromium | Ping's own player and its adaptive buffer, stereo |
| Input latency | Controllers polled every 16 ms | Sent the moment the platform reports it (the connection's thread is woken for it) |
| Tokens | Kept in Electron's store, written to its debug log | An owner-only file, never logged |
| Friends | Online friends, in a sidebar refreshed every 30 s | Online friends and what they play, on the Xbox page and `ping xbox friends`, asked for when looked at (Xbox Live allows 30 requests in five minutes) |
| Achievements, profile | Yes | No: they are not streaming |
| Microphone (party chat) | Prepared, not finished | No |
| Touch controls | Yes | No: desktops have no touch screen |

## How it works

`pingpong-xbox` is the protocol, `ping_core::xbox` where it meets Ping's
platform layer ([architecture.md](architecture.md)):

1. **Signing in**: Microsoft's device code (OAuth 2.0) with the Xbox app's
   public client, as Greenlight signs in. The Microsoft account's token buys
   an Xbox Live user token, which buys XSTS tokens for the web APIs and the
   streaming service, which buys a streaming token for consoles (`xhome`)
   and one for the cloud (`xgpuweb`, or `xgpuwebf2p`). Each is renewed from
   the one before as it nears expiry, and all are kept in `xbox.json` in
   Ping's data folder, readable only by you.
2. **A session**: the streaming service of the account's region starts it
   on the console or a cloud server (`POST /v5/sessions/home/play`), and Ping
   waits for it: the console wakes, the cloud may queue, a cloud session
   asks for one more token. Ping then sends its WebRTC offer and its ICE
   candidates through the service and gets the console's back.
3. **The connection**: WebRTC on [str0m](https://github.com/algesten/str0m),
   pure Rust, on one UDP socket and one thread: H.264 and Opus in, and four
   data channels (input, control, message, chat). Input reports are the
   web client's binary format; the message and control channels are JSON.
4. **The stream**: frames go from the connection's thread straight to the
   decoder, Opus to the player, and the window's keys, mouse and the
   controllers' states back the other way. The session is kept alive every
   30 seconds and deleted when the stream ends.

An Xbox stream does not travel in pingpong's post-quantum tunnel: the
console speaks WebRTC, which is encrypted with DTLS-SRTP. Ping talks to
Microsoft's sign-in, Xbox Live and the streaming service over HTTPS, and to
the console or the cloud server directly.

Limits: IPv4 only; H.264 only (what the console sends web clients); the
console draws its own dialogs and keyboard into the picture.

## What is verified

Without an Xbox or an account, against a mock of both (below):

- Signing in, the code, waiting for it, renewing tokens, a sign-in revoked.
- The console list, waking a sleeping console before its stream.
- A console's stream and a cloud game's (through its queue and the
  transfer token), end to end: picture, sound, controllers, keys as a
  controller, rumble back, a second controller, the console ending the
  stream, Ping ending it.
- 40% video loss: the keyframe asked for both ways, the picture recovering.
- In Ping's own window on macOS (VideoToolbox, Metal) and on Linux
  (FFmpeg, wgpu), at 60 frames a second, with the input drawn by the mock
  in the picture. Windows: type-checked.
- Measured on an Apple silicon Mac against the mock on the same machine
  (release builds, loopback, so no network is in it; the mock's 640×360
  picture): 60 frames a second received, decoded and shown; 0.4 ms from a
  frame's first packet to the decoder; 1.3 ms to decode.

Not yet verified against Microsoft's services or a real console. What
Greenlight does today is the reference, and the services change without
notice. Anyone with a Microsoft account can check the sign-in, the
console list and, where it is offered, a free-to-play cloud game, without
a console.

## Testing without a console

`xbox-mock` (the `pingpong-xbox-mock` crate) is Microsoft's sign-in, Xbox
Live, the console list and the streaming service, and a console at the end
of a real WebRTC connection, all on this computer. It streams a test
picture: a moving bar, a frame counter, and a panel that shows the input it
receives (buttons lit, the sticks, the triggers, the mouse, the last key),
with a tone. It rumbles when A is pressed.

```sh
cargo run -p pingpong-xbox-mock --bin xbox-mock     # prints PING_XBOX_MOCK=http://127.0.0.1:47920
PING_XBOX_MOCK=http://127.0.0.1:47920 PING_DATA_DIR=/tmp/ping-test ping xbox sign-in
PING_XBOX_MOCK=http://127.0.0.1:47920 PING_DATA_DIR=/tmp/ping-test ping xbox stream "Mock Xbox" --stats
```

`PING_XBOX_MOCK` sends every request Ping makes for Xbox to the mock
instead of Microsoft. The end-to-end tests run the client against it
(`cargo test -p pingpong-xbox-mock`), and `tools/linux/xbox-test` runs
Linux Ping against it in the container and saves a picture.
