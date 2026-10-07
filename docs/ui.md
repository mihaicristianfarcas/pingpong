# The windows: Ping and Pong (GPUI)

Both apps are drawn with [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui), Zed's UI framework, in
one design language that lives in `pingpong-ui`: quiet neutral surfaces, one
hairline weight, a sidebar whose selected row is a glass pill, settings as
cards of rows that each say what they do, colour kept for status (green
live, amber attention, red danger), and Ping's orange as the only accent.
Light and dark follow the system (`PINGPONG_APPEARANCE=light|dark`
overrides it).

- **`pingpong-ui`**: the theme (`Theme`, `Ink`, `Radius`, `Type`,
  `Metrics`), one line-icon family (24×24, 1.75 pt round strokes), the
  controls (buttons, icon buttons, switch, segmented control, select menu,
  slider, stepper, text field with IME, menus, sheets, cards and setting
  rows, chips, status dots, spinner, keycaps, stats, notices), and Markdown
  rendering for the agents' words. Also what both apps need of the desktop:
  the menu bar a Mac app has (`menus`), the tray icon (`tray`: the menu
  bar's status item, the taskbar's notification area), one running copy
  per data folder (`instance`), starting at login (`login`), the About
  panel and staying out of the Dock (`desktop`), and the update check's
  notice, sheet and settings (`updates`, over `pingpong-update`).
- **`ping-app`**: Ping, the client.
- **`pong-app`**: Pong's own window. The host runs without it (the service
  on Windows, Pong.app or a user unit elsewhere); the web UI stays, for
  headless hosts and for doing it from another computer.

## Ping

A sidebar: **Hosts**, **Agents**, the open **Sessions** (when there are
any), then the settings pages (**General**, **Video**, **Audio**, **Input**,
**Agent setup**); at its foot, the app's version or "Looking for hosts…",
and above it a line when a newer Ping exists. ⌘1, ⌘2 and ⌘, go to Hosts,
Agents and General (Ctrl on Windows and Linux).

On a Mac the menu bar is a Mac app's: **Ping** (About, Check for Updates,
Settings, Services, Hide, Quit), **File** (Add Host ⌘N, Look for Hosts
Again ⌘R, Close Window ⌘W), **Edit** (cut, copy and paste in the text
fields), **View** (the pages), **Window** (Minimize ⌘M, Zoom, the window
list) and **Help** (the documentation, the logs, reporting an issue).

- **Hosts**: a card per host with its status. Hovering says what a click
  does (Stream, Wake, Pair); the card's "…" or a right click has the rest
  (Steam Big Picture, Wake, the web UI, Unpair). Under the cards, what a
  click streams at, and how to change it.
- **Settings**: each row names a setting and says in a line what it does.
- **Agents** starts agent sessions; **Sessions** lists them and your own
  desktop streams. A session's page has only the session's title and its
  buttons along the top: its state is in the sidebar, the rest in the side
  panel ([ai-agents.md](ai-agents.md#2-work-with-it-sessions)).
- **General**: the update check ([usage.md](usage.md#updates)).
- **A stream** opens in a window of its own (see [usage.md](usage.md)).
  A stream window that goes away without ending its stream is noticed
  within a quarter of a second, and the stream ends with it.

Code: `ping-app/src/app/` (the window, sidebar, sheets, the demo driver),
`hosts.rs`, `settings.rs`, `agents/`, `chat/` (an agent session: the model
in `mod.rs`, the page in `view.rs`, the side panel in `panel.rs`).

## Pong

A sidebar: **Overview**, **Devices**, the settings (**General**, **Video**,
**Network**, **AI agents**), **Logs**; the host's name and state at its foot,
and above it a line when a newer Pong exists.

The window belongs to Pong's icon, in the menu bar (a Mac) or the taskbar's
notification area (Windows): the app keeps the host's state and the update
check, and the window comes and goes (`pong-app/src/background.rs`,
`link.rs`). With no window open the host is asked every three seconds, not
every second, and not for its devices or the agent's log. A new pairing
request is a system notification unless the window is in front. On a Mac
the app has no Dock icon; on Linux, with no tray, closing the window quits.

- **Overview**: a pairing request as a banner when there is one; the session
  (who, the mode, End Session; an agent's with Pause, Hand Back, Stop) with
  its numbers (frames, bitrate, host latency, round trip, loss,
  recoveries); this host (ID, tunnel port, internet address, the router's
  port mapping, devices, version); the agent's recent actions.
- **Devices**: pairing requests first, each with its PIN field (Return
  pairs) and what the device may do once paired; then the paired devices
  and agents, each with a button that names what it may do and opens the
  sheet that sets each permission, and unpairable.
- **Settings** save as they change (text fields on Return). Encoder settings
  a Mac host does not have (NVENC, AV1) are not shown there. General's last
  section, **This window**, is the window's own: its icon at login and the
  update check.
- **Logs**: the last 400 lines of the host's log, following its end.

Code: `pong-app/src/app/` (one module per page), `api.rs` (the web API
client), `worker.rs` (its thread).

### How Pong's window reaches the host

Over the host's web API, HTTPS on localhost, trusting only the certificate
the host made for itself (`web-cert.pem` in its data folder), with a bearer
token:

- **The local token.** The host writes a fresh one at every start
  (`local-token` in its data folder), readable only by its own user on a
  Mac or Linux, where the host runs as that user. The window uses it and
  asks nothing.
- **An app token, on Windows.** The host is a service there, and its token
  is readable by SYSTEM and Administrators only, so a window that is not
  elevated **signs in once** with the web UI's admin account (creating it
  if there is none yet). It gets an app token, which it keeps in the user's
  profile (`%APPDATA%\Pong\app-token`) and the host keeps hashed
  (`app-tokens.toml`). Signing out drops it on both sides.

`POST /api/app-token` (user, password, optional `setup` to create the
account, a name) and `DELETE /api/app-token` are the API's only
window-specific endpoints; every other endpoint takes the bearer token as it
takes the web UI's cookie.

## Logs

Each program writes its own, and never a PIN, password, key or token:

| Program | Where |
|---|---|
| Ping | `~/Library/Logs/Ping/ping.log` (macOS), `%LOCALAPPDATA%\Ping\Logs\ping.log` (Windows), `~/.local/state/ping/ping.log` (Linux); the previous two launches' beside it |
| Pong's window | the same places, `Pong/pong-app.log` |
| Pong (the host) | `logs/pong.log.DATE` in its data folder (`C:\ProgramData\Pong\logs` for the service) |
| `ping`, `ping-agent`, `Ping mcp` | stderr |

`RUST_LOG` narrows or widens any of them (for example
`RUST_LOG=info,ping_core::input=trace` logs each input event sent).

## GPUI

GPUI is a git dependency pinned to one Zed revision (in the workspace
`Cargo.toml`). Its Metal shaders are compiled when the app starts (the
`runtime-shaders` feature, on by default), so a build needs only the Command
Line Tools; the app-bundle scripts compile them ahead of time when Xcode's
Metal toolchain is installed. `vendor/ztracing*` are no-op stand-ins
(Apache-2.0) for Zed's GPL-licensed profiling shim, which GPUI names but does
not use. On Linux GPUI needs fontconfig, freetype, xkbcommon, Wayland and
xcb development packages (see [install.md](install.md#building-from-source)).

## Checking it without clicking

`PING_UI_DEMO` and `PONG_UI_DEMO` drive either window through a list of
steps and can save it as a PNG (on a Mac, `screencapture` of the window), so
a change to the UI can be checked, and compared with before, without a
person:

```sh
PING_UI_DEMO=settings=video,snapshot=/tmp/video.png,quit target/debug/ping-app
PING_UI_DEMO=agents=sample,snapshot=/tmp/session.png,quit target/debug/ping-app
PONG_UI_DEMO=sample,devices,snapshot=/tmp/devices.png,quit target/debug/pong-app
PONG_UI_DEMO=sample,devices,permissions=macbook,snapshot=/tmp/may.png,quit target/debug/pong-app
```

Ping's steps: `settings[=general|video|audio|input|agents]`,
`agents[=setup|sample|sample-live|sample-ask]`, `chat=MESSAGE` (to the open
session, or a new one on the first host the agent may use), `login`,
`pair=HOST:PORT`, `add`, `alone` (hosts found on the network are left
out: the Hosts page as it is with none), `size=WxH` (the window that size,
in points, to see a long page whole), `menu=NAME`, `unpair=NAME`, `stream=NAME`,
`desktop` (that stream's page, while it runs), `stop-after=SECS`,
`wait=SECS`, `close-login`, `vanish-login`, `step=N`, `panel-end`,
`update` and `update-main` (the update check says there is a newer release,
or newer commits on main), `updates` (its sheet), `menus` (the menu bar, to
the log), `snapshot=PATH`, `quit`. `PING_UI_DEMO_SCREEN=PATH` gives the
sample sessions a screenshot to show.

Pong's steps: a page's name (`overview`, `devices`, `general`, `video`,
`network`, `agents`, `logs`), `sample` (a made-up session, request,
devices and settings), `signin`, `setup`, `offline`,
`signin-as=USER:PASSWORD`, `update`, `update-main`, `updates`,
`permissions=NAME` (the permissions sheet of the paired device called
NAME), `menus` (the menu bar and the tray icon's menu, to the log),
`snapshot=PATH`, `close`
(the window closes as its button closes it; the app stays behind its icon),
`quit`. `PONG_DATA_DIR` points the window at a test host's data folder,
and keeps the window's own files there too; `PONG_APP_URL`, `PONG_APP_CERT`
and `PONG_APP_TOKEN` at another host altogether.

With either variable set, the update check asks GitHub nothing on its own
(`update` makes up its answer), and the app runs beside a copy that is
already running rather than showing it.

Use an empty `PING_DATA_DIR` for checks, so they neither read nor change
your own hosts and settings. An occluded window is not redrawn, so the demo
driver brings its window to the front first.
