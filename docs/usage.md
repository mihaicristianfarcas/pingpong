# Using Ping and Pong

Once [installed and paired](install.md), streaming is a click. This page is
the rest: the shortcuts, the settings on both sides, the clipboard, waking a
host, reaching it from away, and what to check when something does not work.

## Streaming

Click a paired host on Ping's **Hosts** page. The stream opens in a window of
its own — full screen by default — at the resolution, frame rate and bitrate
set under **Video**; the host sets up a display in exactly that mode for the
session. Ping's own window stays open beside it. The card's **…** menu (or a
right click) has more: **Steam Big Picture** (open Steam's Big Picture on
the host, and close it after), **Wake**, the host's **web UI**, **Unpair**.

While the stream has the keyboard and mouse, every key goes to the host —
system shortcuts included — except Moonlight's chords, **Ctrl+Alt+Shift**
and a key (Ctrl+Option+Shift on a Mac keyboard):

| Keys | Does |
|---|---|
| Ctrl+Alt+Shift+Q | Stop streaming |
| Ctrl+Alt+Shift+Z | Release the mouse and keyboard, or capture them again |
| Ctrl+Alt+Shift+X | Switch between full screen and a window |
| Ctrl+Alt+Shift+D | Minimise the stream |
| Ctrl+Alt+Shift+S | Show or hide the statistics |
| Ctrl+Alt+Shift+M | Switch the mouse between game and desktop mode |
| Ctrl+Alt+Shift+V | Type this computer's clipboard on the host |
| Ctrl+Alt+Shift+T | Watching an AI agent: take over its keyboard and mouse, or hand them back |

**The mouse is automatic.** On the desktop the pointer is yours, drawn by
Ping in the host's pointer shape and sent as positions; when a game hides
the pointer and takes the mouse, Ping switches to relative motion (what
games read), and back when the game lets go. Ctrl+Alt+Shift+M overrides
that for a game that needs it; press it again to follow the host.

**Controllers** plug in on the host as Xbox 360 pads (Windows, with
ViGEmBus), one per controller, with rumble; the Xbox/Guide button reaches
the host too. With **Controller as a mouse** on (Input settings), holding
Start switches the controller to driving the mouse, as in Moonlight.

**Taking over.** One person streams a host at a time. A second client asking
for the host takes over the session (the first is told) unless the host's
**Let a client take over a session** is off, in which case it is refused.

### The statistics

Ctrl+Alt+Shift+S (or `ping stream --stats`) shows Moonlight's figures —
incoming, decoded and rendered frame rates, bitrate, frames dropped,
network round trip, host processing, decode and render times — and one more:
**host capture to screen**, the time from the host capturing a frame to it
being on your display, measured with the two machines' clocks related
through the stream itself. A note in the corner warns while the network is
losing frames or lagging (**Connection warnings**).

## Ping's settings

Changes to the streaming settings apply to the next stream. ⌘, (Ctrl+, on
Windows and Linux) opens them.

| Page | Setting | What it does |
|---|---|---|
| General | Look for updates | New releases (the default for a packaged release), releases and main (the default for a build from a checkout: also commits on `main` newer than the build's), or off. See [Updates](#updates) |
| | Check for Updates | Asks GitHub now, whatever the setting |
| Video | Resolution | This display's native size (the default), the scaled desktop size (Mac), or a custom size |
| | Frame rate | Frames per second the host sends; up to this display's maximum |
| | Video codec | Automatic (HEVC when both ends can), HEVC, or H.264 |
| | Display mode | Full screen or a window |
| | Bitrate | Automatic (Moonlight's table for the resolution and frame rate), or a fixed figure |
| | V-Sync | Off: frames shown the moment they are decoded, tearing allowed |
| | Frame pacing | One frame per display refresh (Moonlight's pacer): smoother, about a refresh more latency. Off by default, as in Moonlight |
| | Performance statistics | The statistics from the start of each stream |
| | Connection warnings | The note in the corner |
| | Import from Moonlight | Copies Moonlight's own settings (its preferences on macOS, the registry on Windows, its `.conf` on Linux; never its keys) |
| Audio | Stream audio, Channels | Play the host's sound here: stereo, 5.1 or 7.1 |
| | Play on the host too | Otherwise the host's speakers stay quiet during a session (Windows host with Steam Streaming Speakers) |
| | Mute in the background | Silence the stream while its window is not in front |
| Input | Use ⌘ as the Windows key | On a Mac: Command reaches a Windows host as the Windows key (a Mac host always gets Command) |
| | Share the clipboard | See [below](#sharing-the-clipboard) |
| | Controller as a mouse | Hold Start to drive the mouse with a controller |

Settings are kept in `settings.toml` in Ping's data folder, beside the paired
hosts (`hosts.toml`) and this device's keys:

| System | Ping's data folder |
|---|---|
| macOS | `~/Library/Application Support/Ping` |
| Windows | `%APPDATA%\Ping` |
| Linux | `~/.config/ping` (`$XDG_CONFIG_HOME/ping`) |

`PING_DATA_DIR` points Ping (and `ping`, `ping-agent`) elsewhere.

## Pong's window and web UI

Pong's window (**Pong Control** on a Mac, **Pong** in the Windows Start
menu) and its web UI (`https://HOST:47802`) do the same things: see and end
the session, pair and unpair devices, change settings, read the log. The
window is for the host's own screen; the web UI for a headless host or
another computer. On Windows the window signs in once with the web UI's
admin account.

**Pong's icon** sits in the menu bar on a Mac and in the taskbar's
notification area on Windows, with or without the window open. Its menu
says what the host is doing (running, streaming to whom, not running),
opens the window, names a device asking to pair (a notification says so
too), and says when a newer Pong exists. On a Mac, Pong's window app has no
Dock icon at all: the menu bar icon is where it lives. Closing the window
leaves the icon; **Quit Pong Control** in its menu removes it. Either way
the host keeps running: it is a service of its own (PongService, or
Pong.app). Linux has no single tray to put an icon in, so there Pong's
window is an ordinary app: closing it quits it.

| Page | Setting | Default | `config.toml` |
|---|---|---|---|
| General | Name (as clients see it) | the computer's name | `name` |
| | Let a client take over a session | on | `allow_takeover` |
| | Keep this PC's monitors on while streaming | off: the virtual display is the whole desktop | `keep_host_displays` |
| | Share the clipboard with clients | on | `clipboard` |
| Video | NVENC preset | P1 (fastest), as Apollo | `nvenc_preset` |
| | Two-pass encoding | on | `nvenc_two_pass` |
| | Allow HEVC, Allow AV1 | on | `allow_hevc`, `allow_av1` |
| | Maximum bitrate, maximum frame rate | the client's choice | `max_bitrate_kbps`, `max_fps` |
| | Send pacing | 800 Mbit/s | `pace_mbps` |
| | Adapt the bitrate to the network | on | `adaptive_bitrate` |
| Network | Allow streaming over the internet | on | `internet_access` |
| | Ask the router to forward the port | on | `port_mapping` |
| | Ports: tunnel (UDP), pairing, web UI | 47800, 47801, 47802 | `port`, `pairing_port`, `web_port` |
| AI agents | Allow paired AI agents | on | `agents` |
| | Hold after local input | 10 s | `agent_local_input_hold_secs` |

General's last section, **This window**, is the window's own, not the
host's; it is kept per user, in `window.toml` beside the app token
(`%APPDATA%\Pong` on Windows, Pong's data folder elsewhere):

| Setting | Default | What it does |
|---|---|---|
| Show Pong's icon at login | on after `host-deploy.ps1` (Windows) | The icon at sign-in, without the window (a LaunchAgent on a Mac, the user's `Run` key on Windows; not on Linux) |
| Look for updates | as Ping's | As Ping's, for Pong |

A changed name or port applies after Pong restarts (the window offers to).
Pong's data folder holds `config.toml`, its identity, the paired clients,
the web UI's certificate and account, and `logs/`:

| System | Pong's data folder |
|---|---|
| Windows | `C:\ProgramData\Pong` |
| macOS | `~/Library/Application Support/Pong` |
| Linux | `~/.config/pong` |

`PONG_DATA_DIR` points Pong elsewhere.

## Updates

Ping and Pong's window say when there is something newer than the copy
that runs: a line at the foot of the sidebar ("Ping 0.7.0 is available"),
and on Pong's icon, its menu. Clicking it says what it is and how to get
it: the release's page, or `brew upgrade --cask ping` for a copy Homebrew
installed, or, for a build from a checkout, the commits on `main` it does
not have. Nothing is downloaded or installed by the apps.

They ask GitHub's public API, without an account: a few seconds after
starting when the last answer is more than a day old, and once a day while
they run. What they send is what any HTTPS request shows (this computer's
address) and the version in the `User-Agent`. **Look for updates: Off**
stops it; **Check for Updates** (in the app's menu, and in General) still
asks once. The last answer is kept in `update.toml` in the app's data
folder.

## Sharing the clipboard

Copy on one side, paste on the other: text, images, and files and folders,
both ways, for as long as your stream runs. Ping asks for it (**Share the
clipboard**, on by default; `ping stream --no-clipboard` for one stream),
and the host agrees unless its **Share the clipboard with clients** is off.

- Each end watches its own clipboard and sends each new copy: files if there
  are some, else text, else an image. Files arrive in a temporary folder
  (`Ping Clipboard` or `Pong Clipboard`, replaced by the next copy) and are
  put on the clipboard as files, so pasting in Finder or Explorer copies
  them from there.
- Copies travel inside the tunnel, chunked and acknowledged, under a rate
  cap of half the stream's bitrate so the video goes first; up to 256 MB.
- **Password managers' copies stay where they were made**: anything marked
  concealed is never sent.
- AI agents' sessions and people watching them never share the clipboard.

Ctrl+Alt+Shift+V still types the clipboard's text on the host, as Moonlight
does, for places that do not take a paste.

## Wake-on-LAN

Pong announces its network adapters on the local network, and Ping
remembers them, so an offline host can be woken: **Wake** in the host's
menu, a click on an offline host, or `ping wake NAME`. The host must be on a
wired connection with Wake-on-LAN enabled: in its firmware (often called
"Resume by PCI-E device" or "Wake on LAN"; with ErP off), and in the network
adapter's settings (power-saving features such as Energy-Efficient Ethernet
off). Ping must have seen the host on the local network at least once.

## Away from home

Nothing to set up: with **Allow streaming over the internet** on (the
default), a paired Ping finds the host through the DHT and connects directly
through both NATs, from anywhere. The first connection of a session can take
a few seconds; while Ping is open it keeps the path warm. If both networks
use symmetric NAT, forward UDP 47800 to the host on its router. How it
works: [networking.md](networking.md).

## When something does not work

- **Ping finds no hosts (macOS).** Allow Ping on the local network: System
  Settings > Privacy & Security > Local Network.
- **Windows asks about the firewall.** Allow Ping (and `ping.exe`) on private
  networks. The host's firewall rules are added by `pong install`.
- **"The host did not answer."** Is Pong running (its window says; on
  Windows, `PongService` in Services)? Is this device still paired (the host's
  **Devices**)?
- **"The host could not create its virtual display"** (Windows): SudoVDA is
  not installed, or Apollo's service is holding it — stop ApolloService.
- **A Mac host ignores the keyboard and mouse**, or streams nothing: give
  Pong Accessibility and Screen Recording in System Settings > Privacy &
  Security, then restart it.
- **A Linux host under Wayland shows a dialog every time**, or ignores input:
  answer the desktop's sharing dialog with **Allow Remote Interaction** on;
  the restore token then saves asking again.
- **Stutter every 20–30 s on a Mac on Wi-Fi**: AWDL (AirDrop, Continuity) is
  a common cause; see [benchmarks.md](benchmarks.md#long-sessions-and-loss).
- **Logs** say what happened (never a PIN, password, key or token):
  [ui.md](ui.md#logs) lists where each program writes. `RUST_LOG` widens
  them, e.g. `RUST_LOG=info,ping_core::stats=debug` for a line of
  statistics a second.

The command line (`ping`, `pong`, `ping-agent`) is in [cli.md](cli.md).
