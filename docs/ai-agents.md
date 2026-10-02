# AI agents on your hosts (computer use)

An AI agent can use a paired host the way you do through Ping: it sees the
screen, and it drives the keyboard and mouse. It works over the same
post-quantum tunnel, the same stream and the same input path, from anywhere
Ping reaches. You hand it a task from Ping's **Agents** page, or give an
agent you already use (Claude Code, Codex, Claude Desktop, Cursor) your
hosts as tools through `Ping mcp`. Either way you can watch it, take over,
pause or stop it, and the host holds it to rules of its own.

## Using it

### 1. Let the agent use a host

The agent is a client of its own: it pairs once per host, like a device, and
the host lists it as an **AI agent**.

- Ping: **Allow…** beside the host, on the Agents page (under **Your
  hosts**) or in **Agent setup** (under **Hosts the agent may use**). Type
  the PIN into Pong's window or web UI, as for Ping.
- CLI: `ping pair-agent HOST`.
- By hand (headless hosts): `ping-agent identity` prints the agent's keys;
  `pong add-client NAME X25519 MLKEM --agent` on the host, and
  `ping-agent add-host NAME ADDR X25519 MLKEM` with `pong identity`'s keys.

### 2. Work with it: sessions

On Ping's **Agents** page, say what you need, pick the host, the model and
when to ask for your go-ahead, and press Return. That opens a **session**: a
conversation with the agent about one host, listed in the sidebar under
**Sessions** with its state (Working, Waiting for you, Paused, Connected,
Idle). You and the model take turns: ask it something, have it do
something, look over its shoulder, do a bit yourself, ask again.

- **The model answers in words** (Markdown) and **uses the host when a
  message needs it**. A turn's actions fold into one line ("Used the
  computer · 6 actions") that opens to each action, its result and the
  screen after it.
- **The connection is the session's**: made at the first action (or when
  you log in), kept between turns so the next turn finds the desktop as the
  last one left it, let go after 20 idle minutes (the next message connects
  again), ended with the session.
- **What the agent sees** (the eye at the top) is a side panel: the screen
  after the latest action with a ring where it landed — the picture the
  model got, not a second stream — every step so far, the model's plan as
  it ticks it off, and the session (the host, the agent and its model, who
  has the keyboard and mouse, the connection, this turn's actions and
  minutes left, tokens). The top of the page holds only the session's title
  and its buttons.
- **Log In** opens the session's desktop in a stream window beside the
  agent. **Ctrl+Alt+Shift+T** takes the keyboard and mouse and gives them
  back; closing the window leaves the session running.
- **Pause** holds the agent before its next action, **Stop** stops the turn
  (the session stays), **End Session** ends the conversation and lets the
  host go.
- **Your go-ahead for risky steps** (the default; the hand beside the
  message field switches to every step, or never). Before it deletes files
  or data, spends money, sends or posts for you, changes account, security
  or system settings, installs software or types a password, the model asks
  (with exactly what it will do and why). Rules also catch what it types and
  the keys it presses, whether it asked or not: commands that delete or
  overwrite for good (`rm -rf`, `del /s`, `Remove-Item -Recurse`, `format`,
  `git push --force`, `DROP TABLE`, `curl … | sh`, …), what looks like a
  password, a key or a card number, and Shift+Delete, Ctrl+Alt+Delete,
  emptying the Trash or locking the screen. A click is never judged by rule:
  only the model knows what is under it. A question waits up to 15 minutes
  (not counted against the turn); no answer is a no. A yes to the model's
  own request covers the next few risky actions that carry it out (2
  minutes, 3 actions), so you are not asked twice.
- **Notifications** (macOS): when a session is not on screen, Ping tells you
  when the agent asks for your go-ahead (with **Allow** and **Don't** on the
  notification), waits at a sign-in or administrator prompt, is done, or
  stopped.

Your own desktop streams are sessions too: a host you stream from the
**Hosts** page is listed under **Sessions** while the stream is up. You can
stream a host yourself at any time, with or without an agent session on it.

### Who does the thinking

Ping runs no model of its own. Pick one under **Agent setup** (the **Model**
menu):

| Provider | How it runs | What you need |
|---|---|---|
| Codex (ChatGPT plan) | `codex exec`, with pingpong's tools only | Codex installed, `codex login` with ChatGPT |
| Claude Code (Claude plan) | `claude -p`, with pingpong's tools only | Claude Code installed and signed in |
| Anthropic API key | Ping drives Claude's computer toolset itself | a key (`ANTHROPIC_API_KEY`, or saved in Agent setup) |
| OpenAI API key | Ping drives OpenAI's computer tool itself | a key (`OPENAI_API_KEY`, or saved) |
| OpenRouter API key | the actions offered as functions | a key; any model with tools and images (`:free` models cost nothing) |
| OpenAI-compatible endpoint | the same, against your URL | Ollama, LM Studio, a gateway |

A turn stops after **60 actions or 15 minutes** by default; both, the
display size (1280x800 by default) and when to ask for your go-ahead are
settings. `ANTHROPIC_BASE_URL` and `OPENAI_BASE_URL` point the API loops at
a gateway or proxy, as the makers' SDKs allow.

**What is kept.** Ping keeps sessions in memory only; each has a working
folder under `agent/conversations/` in Ping's data folder that is removed
when it ends. Codex and Claude Code keep a session's thread themselves,
screenshots included — that is how the next turn resumes it (Codex under
`~/.codex/sessions/`, Claude Code under `~/.claude/projects/`) — and ending
the session does not remove those. One-off runs (`ping-agent run`) keep
nothing. Whatever the provider, the screenshots go to the model's maker as
the model's input.

From the command line:

```sh
ping-agent providers                  # what can run here
ping-agent run --host gaming-pc --provider codex "Open Notepad and write a shopping list"
ping-agent run --host gaming-pc --provider anthropic --max-actions 30 "…"
ping-agent run --host gaming-pc --provider codex --approvals every "…"   # every action asks y/N
ping-agent converse --host gaming-pc --provider codex "What is open?" --then "Close it"
echo "$KEY" | ping-agent set-key openrouter
```

The full list of flags is in [cli.md](cli.md#ping-agent--ai-agents).

### 3. Or use your hosts from another agent (MCP)

`Ping mcp` is an MCP server on stdin/stdout that acts as this device's
agent:

| System | Command |
|---|---|
| macOS | `/Applications/Ping.app/Contents/MacOS/Ping mcp` |
| Windows | `%LOCALAPPDATA%\Programs\Ping\Ping.exe mcp` |
| Linux | `ping-app mcp` |
| A build | `ping-agent mcp` |

**Agent setup**, under *Your hosts in other agents*, lists the agents on
this computer and adds the server to any of them (**Add**), takes it out
again (**Remove**), or points an entry left by another copy of Ping at this
one (**Update**). From a terminal, `Ping mcp install` does the same:

```sh
Ping mcp status                       # each agent, and whether it has the server
Ping mcp install codex claude-code    # or --all: every agent found here
Ping mcp uninstall opencode
```

| Agent | `APP` | Where the entry goes |
|---|---|---|
| Claude Code | `claude-code` | `~/.claude.json` (or `$CLAUDE_CONFIG_DIR/.claude.json`) |
| Codex | `codex` | `~/.codex/config.toml` (or `$CODEX_HOME`) |
| OpenCode | `opencode` | `~/.config/opencode/opencode.json`, with a 60 s timeout |
| Gemini CLI | `gemini` | `~/.gemini/settings.json` |
| Cursor | `cursor` | `~/.cursor/mcp.json` |
| GitHub Copilot CLI | `copilot` | `~/.copilot/mcp-config.json` |
| Factory Droid | `droid` | `~/.factory/mcp.json` |
| Amp | `amp` | `~/.config/amp/settings.json` (`amp.mcpServers`) |
| Kiro | `kiro` | `~/.kiro/settings/mcp.json` |
| Claude Desktop | `claude-desktop` | `claude_desktop_config.json` in its settings folder |
| Diri | `diri` | each of Diri's agent accounts with a settings folder of its own; the others use Claude Code's and Codex's |

The entry is called `pingpong`. The rest of each file stays as it was (its
keys in their order, Codex's comments). A file with comments in its JSON
(OpenCode's `opencode.jsonc`) is not rewritten: add the entry there by hand.
An agent reads its servers when a session starts, so start a new one. By
hand, in any agent:

```sh
claude mcp add --scope user pingpong -- /Applications/Ping.app/Contents/MacOS/Ping mcp
codex mcp add pingpong -- /Applications/Ping.app/Contents/MacOS/Ping mcp
# elsewhere: {"mcpServers": {"pingpong": {"command": "…/Ping", "args": ["mcp"]}}}
```

The tools, named as in Claude's computer-use toolset:

- `list_hosts`, `connect` (host, width, height), `disconnect`, `wake_host`
- `screenshot`, `zoom` (a region, enlarged)
- `left_click`, `right_click`, `middle_click`, `double_click`,
  `triple_click` (at `coordinate`, with modifier keys in `text`)
- `left_click_drag`, `mouse_move`, `left_mouse_down`, `left_mouse_up`,
  `scroll`
- `type`, `key` (xdotool names: `ctrl+s`, `Return`, `alt+Tab`, `super`),
  `hold_key`
- `wait`, `cursor_position`, `wait_for_control`, `session_status`
- `share_plan` (the model's plan, for the person watching)

Every action answers with a screenshot of the screen once it has settled.
After 10 minutes without a call the server lets the host go; the next call
connects again.

### 4. Watch, take over, stop

- **Log In** (a session's page), or `ping watch HOST`, opens the agent's
  screen in a stream window (waiting up to 25 s for an agent still
  starting). **Ctrl+Alt+Shift+T** takes the keyboard and mouse; pressing it
  again hands them back.
- **While you have them, the agent waits** — up to 15 minutes, not against
  the turn's minutes. Its next action is then not done: the model gets the
  screen as you left it and decides again. The same goes when you pause it
  (in Ping or Pong), use the host's own keyboard or mouse, or a secure
  screen is up. **Stop** or **End Session** end the wait at once.
- **Pong's window and web UI** show an agent's session — who watches, what
  holds the agent, and each action as the agent described it — with
  **Pause**, **Resume**, **Hand back** and **Stop**. Under **Devices**, each
  agent's access: **See and control**, **See only** or **Off** (a change
  applies to a running session at once). Under **Settings > AI agents**:
  whether agents are allowed at all, and how long someone using the host's
  own keyboard or mouse holds an agent (10 s).

## The rules the host holds agents to

Pong enforces these, whatever the agent or its model does:

1. **Never over a person.** An agent is refused while a person streams, even
   with take-over allowed.
2. **A person always wins.** A person who starts a stream takes over from an
   agent, even with take-over off.
3. **A secure screen is a person's.** On the sign-in screen, the lock
   screen, a UAC prompt or Ctrl+Alt+Del, the agent's input is held.
4. **Someone at the host comes first.** Input at the host itself holds the
   agent until 10 s after it stops. On a Windows host whose monitors the
   session turned off, it ends the agent's session instead: the person there
   would otherwise sit at a dark screen.
5. **Access per agent.** See only: screenshots, no input. Off: refused.
6. **Watchers and take-over.** People may watch an agent's session; a
   watcher who takes over drives the host while the agent's input is held.
7. **Everything is written down.** The agent says what each action is, and
   Pong logs it and shows it.

The agent hears why it is held. Its next action waits for the hold to end
(up to 15 minutes in the runs and sessions Ping starts; 50 s for `Ping mcp`,
whose clients give a call about a minute) and then answers with what
happened and the screen, so the model decides again. If the hold outlasts
the wait, the action fails and the model is told to stop and report.

## Design decisions, and why

**pingpong is the tool, not the agent.** Models change every month, and
people already pay for some: a subscription through the maker's own program,
or a key. A subscription is only ever used through that program (Codex,
Claude Code), as its terms want; Ping never reads their credentials.

**An agent is a client of its own.** It pairs with a separate key, and the
role is sealed with the PIN's key, so the host knows it by key rather than
by what it claims. So the rules cannot be skipped by an agent that forgets
to say it is one; a person can watch the agent from the same computer; and
the agent's access can be changed or revoked without touching the person's.
The agent keeps its identity and hosts in `agent/` in Ping's data folder.

**The display is the model's size.** Windows and Mac hosts make their
virtual display the size the agent asks for (1280x800 by default, the size
Anthropic and OpenAI suggest for computer use). Screenshot, display and
coordinates are then one space: nothing is scaled. A Linux host scales its
screen to it, as it does for Ping.

**A settled screenshot after every action.** Agents look after every step
anyway, and a picture mid-animation misleads them. An action waits at least
150 ms, then until nothing but a caret's blink has changed for 350 ms (a
coarse 16x16-block luma fingerprint), at most 2.5 s. One round trip per
step, not two.

**The tools are Claude's computer toolset's** (`left_click` with
`coordinate`, `key` with xdotool names): models of every maker handle them,
and for Claude they are the trained names. OpenAI's actions and chat-model
functions map onto the same code.

**The MCP server is written here** (JSON-RPC over stdio), not taken from an
SDK: the protocol is small, the Rust SDKs change fast, and nothing but
protocol may reach stdout. It answers protocol versions 2024-11-05 to
2025-11-25.

**The CLI agents run with pingpong's tools and nothing else.** Codex runs
with its shell, its own computer use, browser, apps, plugins and sub-agents
off, without the user's Codex config, in an empty directory with a
read-only sandbox; Claude Code with no built-in tools, only pingpong's, and
a strict MCP config. Neither can touch the client machine, only the host.

**The API loops follow each API's own shape**: Anthropic's computer toolset
with adaptive thinking and prompt caching over an append-only history;
OpenAI's Responses API with its computer tool (a safety check stops the run
until the user says go on); for chat APIs the actions as functions and the
screen as an image in a user message, keeping the last three screenshots.

**Keys are kept like the tunnel's private key**: in
`agent/credentials.toml`, readable only by the user; an environment
variable wins over a saved key.

**A run is bounded twice.** The runner stops a run after its minutes and
kills the agent's program; the MCP server enforces the action budget and
the run's end itself, and stops acting once the process that started it is
gone. A Ping that crashes mid-run leaves no agent acting.

**Approvals and pausing go through a folder.** Whatever does the actions —
Ping itself, or the MCP server Codex or Claude Code started — checks the
run's control folder before each input action: a `pause` file holds it; a
step to approve is written as `ask-N.json` and waits for `answer-N`. One
mechanism for every provider, with no socket.

**Risky steps: the model asks, rules back it up** (`ping-agent/src/risk.rs`).
The model sees the screen and knows what a click will do, so it is told
which steps need a go-ahead and has a tool to ask. A model can forget, so
what can be judged without the screen — typed text and key chords — is
judged by rule too, tuned to stay quiet for everyday typing: a gate that
asks all the time gets clicked through.

**Watching is a second recipient of the same encode**: no second encoder,
and the watcher sees exactly what the agent sees. Watching is for agents'
sessions only; a person's session is not watchable.

**Telling someone at the host from the agent.** Pong cannot see where input
comes from on every system, so it compares times: input at the host more
recent than the last the session injected, by more than 300 ms, is someone
else's (a session's first three seconds excepted). It is a heuristic: a
person pressing a key within 300 ms of the agent's own input goes unseen
until their next one.

**Text types at 66 characters a second on a Windows host.** Windows 11's
WinUI text fields stall for a moment after each word and read the Unicode
keystrokes queued meanwhile as whichever came last, so Pong types one
character every 15 ms, on a thread of its own (8 ms garbled; 12 ms and more
typed a 200-character sentence exactly).

**A session is a conversation, and Ping stores none of it.** Keeping
sessions across launches would mean a database of transcripts and
screenshots of a user's computers, with its own retention and security
questions.

## Threat model

It protects against an agent that is confused, or instructed by something on
the screen (prompt injection), acting through pingpong. Such an agent cannot
act while a person streams, over a person, on a secure screen, while someone
uses the host, beyond its access, beyond its run's budget or after its run,
or unseen: every action is in Pong's log, and a person can watch and stop
it.

It does not protect against:

- **A malicious program on the client machine.** It can read the agent's
  key, as it can read Ping's: whoever owns the client owns its pairings.
  Unpair the device on the host.
- **An agent outside Ping's runner with other tools.** Claude Code or Codex
  run interactively with pingpong added as an MCP server still have their
  shell on the client machine; that is their sandbox's business.
- **What an agent does within its access.** It is a real computer. Use See
  only for looking, watch the first runs, and prefer a host account without
  administrator rights for agents' work.

## Status

Verified on a Windows host and a Mac host, and in the Linux container
desktop (`tools/linux/agent-desktop`): pairing an agent, MCP sessions,
Codex and Claude Code runs and sessions, the API loops against a scripted
mock of all three APIs (`tools/linux/agent-tests/mock-llm.py`), OpenRouter's
free models, watching and taking over, the host's rules (a UAC prompt, input
at the host, a person starting a stream, See only set mid-session), typing
Unicode on every host, and approvals across processes.

Not verified yet:

- A model driving a Mac host (only a scripted session), and someone at a
  Mac host during an agent's session (the idle reading itself is
  unit-tested).
- The real Anthropic and OpenAI APIs (checked against the mock only), and
  paid OpenRouter models.
- Claude Desktop, Cursor, Gemini CLI, Copilot CLI, Droid, Amp and Kiro as
  MCP clients: their entries follow each one's documented format, and
  `Ping mcp install` was run only against Claude Code, Codex and OpenCode
  (Claude Code and Codex list the entry, and Claude Code connects to it).
- Sessions with Claude Code (`--session-id`, then `--resume`) and with an
  API key's model: built, not run.

## Tests

Unit tests: `cargo test -p ping-agent -p pong -p pingpong-proto -p
pingpong-pairing`. `tools/linux/agent-desktop` starts a desktop in the Linux
container for agents to work on, and `tools/linux/agent-tests/` has the
helpers used above (a scripted MCP client, a watcher, a person streaming, the
web API, the mock model APIs); see its
[README](../tools/linux/agent-tests/README.md).
