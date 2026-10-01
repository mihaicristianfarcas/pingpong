# Command line

Everything the apps do can also be done from a terminal: streaming and
pairing with `ping`, the host with `pong`, AI agents with `ping-agent`. The
CLIs are built with the apps (`cargo build --release -p ping-core -p pong
-p ping-agent`; the binaries land in `target/release/`).

## `ping` — the client

The same streaming code as the app, with a window of the platform's own.

| Command | Does |
|---|---|
| `ping identity` | Print this client's public keys (X25519 and ML-KEM-768) |
| `ping discover` | List hosts on the local network, paired or not |
| `ping pair HOST[:PORT]` | Pair with a host: shows a PIN to type on the host |
| `ping pair-agent HOST[:PORT]` | Pair this device's AI agent (a key of its own) |
| `ping hosts` | List paired hosts |
| `ping add-host NAME ADDR X25519 MLKEM` | Pair by hand, with the host's keys from `pong identity` |
| `ping remove-host NAME` | Forget a host |
| `ping wake NAME` | Send Wake-on-LAN packets to a paired host |
| `ping stream NAME [FLAGS]` | Stream a paired host |
| `ping watch NAME [FLAGS]` | Watch the AI agent working on a host |

`ping stream` flags (the defaults come from this display, at 60 fps and
Moonlight's bitrate for the mode):

| Flag | |
|---|---|
| `--size WxH` | Stream resolution |
| `--fps N` | Frame rate |
| `--mbps N` | Bitrate (otherwise automatic) |
| `--codec h264\|hevc` | Only this codec |
| `--windowed` | A window, not full screen |
| `--no-vsync` | Present frames at once, tearing allowed |
| `--frame-pacing` | One frame per display refresh |
| `--stats` | Show the statistics from the start |
| `--cmd-is-win` | Send Command as the Windows key (macOS) |
| `--mute-in-background` | Silence the stream while its window is not in front |
| `--audio-channels 2\|6\|8` | Stereo, 5.1 or 7.1 |
| `--no-audio` | No sound |
| `--host-audio` | Play the sound on the host too |
| `--steam` | Open Steam Big Picture on the host for the session |
| `--keep-host-displays` | Leave the host's own monitors on |
| `--no-clipboard` | Do not share the clipboard |
| `--wan-only` | Only the internet path (no LAN, no known addresses): to test it |
| `--via ADDR` | Only this address (repeatable): to diagnose a path |

Ctrl-C ends the stream properly, as Ctrl+Alt+Shift+Q does, so the host puts
its displays back at once. The exit status is 1 when the stream ended for a
reason other than you quitting.

## `pong` — the host

| Command | Does |
|---|---|
| `pong` or `pong host` | Run the host in this session |
| `pong identity` | Print the host's public keys |
| `pong clients` | List paired clients and agents |
| `pong add-client NAME X25519 MLKEM [--agent]` | Pair a client by hand (headless setups), with the keys from `ping identity` or `ping-agent identity` |
| `pong remove-client X25519` | Unpair a client |
| `pong agent-access X25519 control\|view\|off` | What a paired agent may do |
| `pong install`, `pong uninstall` | Windows: install or remove `PongService` and its firewall rules (as administrator) |
| `pong service` | Windows: the service itself (started by Windows) |
| `pong clipboard-agent` | Windows: clipboard sharing as the signed-in user (started by the host for a session) |

Changes made with `add-client`, `remove-client` and `agent-access` apply to
a running host after it restarts; the window and the web UI apply theirs at
once. On Windows the installed host's data folder is private to SYSTEM and
Administrators, so these commands need an administrator's terminal there.

## `ping-agent` — AI agents

| Command | Does |
|---|---|
| `ping-agent providers` | What can run here, which provider is chosen, and whether Jev can help (and with what) |
| `ping-agent set-key PROVIDER` | Save an API key (`anthropic`, `openai`, `openrouter`, `custom`, or `typesafe` for Jev, which is checked with the service at once), read from stdin; an empty line forgets it |
| `ping-agent identity` | The agent's public keys (for `pong add-client --agent`) |
| `ping-agent hosts` | Hosts the agent is paired with |
| `ping-agent add-host NAME ADDR X25519 MLKEM` | Pair the agent by hand |
| `ping-agent run --host NAME [FLAGS] TASK` | Run one task, printing each action |
| `ping-agent converse --host NAME [FLAGS] MESSAGE [--then MESSAGE ...]` | A conversation: each message a turn, on one connection |
| `ping-agent mcp [FLAGS]` | The MCP server on stdin/stdout (what `Ping mcp` runs) |

`run` and `converse` flags: `--host NAME` (the default: the only host the
agent may use), `--provider codex|claude-code|anthropic|openai|openrouter|custom`,
`--model M`, `--effort E`, `--max-actions N`, `--max-minutes N` (run),
`--base-url URL` (run), `--size WxH` (run), `--approvals off|risky|every`
(which steps wait for your yes on the terminal; `--confirm` is `every`),
`--yes` (run: answer yes to the API's safety checks), `--answer yes|no`
(converse: what every question is answered), `--jev off|clicks,endings,model`
(what Jev does this time, over the saved settings; see
[ai-agents.md](ai-agents.md#jev-fast-judgments-beside-the-model)).

`mcp` flags: `--host NAME` (the default host), `--size WxH`,
`--max-actions N`, `--image-dir DIR` (screenshots also saved as files),
`--events FILE` (a JSON line per action), `--no-auto-screenshot`,
`--approvals off|risky|every` with `--control-dir DIR` (where questions and
answers are exchanged; see [ai-agents.md](ai-agents.md#design-decisions-and-why)),
`--hold-wait SECS` (how long an action waits while a person has the
keyboard and mouse), `--data-dir DIR`, `--jev-clicks` (Jev checks each
click, with `--approvals risky`; its key is read from the data folder or
`TYPESAFE_API_KEY`). `--until` and `--owner-pid` bound a run started by
Ping.

## The apps

- `Ping mcp` (or `ping-app mcp`) runs the MCP server, as `ping-agent mcp`.
- On Linux, `ping-app --ping-stream` is the stream's own process, started by
  the app.
- `Pong Control --background` (`pong-app --background`) starts Pong's window
  app as its icon only, without the window: what starting at login does.
  Where there is no tray (Linux) the window opens all the same.
- Starting either app while it runs shows the copy that runs, and adds none
  (one copy per data folder: a `PING_DATA_DIR` or `PONG_DATA_DIR` elsewhere
  is another app).

## Environment variables

For everyone:

| Variable | Used by | Does |
|---|---|---|
| `RUST_LOG` | all | Log filter, e.g. `info,ping_core::stats=debug` |
| `PING_DATA_DIR` | Ping, `ping`, `ping-agent` | Ping's data folder |
| `PONG_DATA_DIR` | Pong, Pong's window | Pong's data folder |
| `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `PING_AGENT_API_KEY` (a custom endpoint) | agents | API keys; win over saved keys |
| `ANTHROPIC_BASE_URL`, `OPENAI_BASE_URL` | agents | Point the API loops at a gateway or proxy |
| `TYPESAFE_API_KEY`, `TYPESAFE_AI_API_KEY` | agents | Jev's key (TypeSafe's, or an OpenRouter key), read in that order; wins over the saved one |
| `TYPESAFE_BASE_URL` | agents | Where Jev is asked, instead of `https://api.typesafe.ai` (or `https://openrouter.ai/api` for an OpenRouter key) |
| `PINGPONG_APPEARANCE` | the apps | `light` or `dark`, whatever the system says |
| `PINGPONG_UPDATE_API` | the apps | Where the update check asks instead of `https://api.github.com` (a mirror, or a test) |
| `PING_SOFTWARE_DECODE` | Ping on Linux | Decode in software even where VA-API is available |
| `PONG_CAPTURE` | Pong on Linux | `x11` or `portal`, where the session type is not clear |

For diagnosis (see [benchmarks.md](benchmarks.md)):

| Variable | Does |
|---|---|
| `PINGPONG_SEND_BATCH=N` | Datagrams per send call (default 16; 0: one at a time) |
| `PINGPONG_RECV_BATCH=N` | macOS client: datagrams per receive call (default 32; 0: one at a time) |
| `PINGPONG_LAN_SHARDS=0` | The client asks for path-sized datagrams on the LAN too |
| `PINGPONG_SERVICE_CLASS=0` | Leave the client's traffic unmarked (not Wi-Fi's voice queue) |

For tests, and the ones only tests need, see
[development.md](development.md#test-hooks).
