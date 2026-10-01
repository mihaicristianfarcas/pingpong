# Development

How to build, test and check pingpong across its three platforms, and the
tools and hooks that make that possible without three desks. The rules for
changes themselves — code style, comments, docs, commits — are in
[AGENTS.md](../AGENTS.md).

## The repository

```
Cargo.toml            the workspace (every crate below but spikes/ and tools/rawinput-probe)
ping-app/             Ping's window (GPUI)                    ─┐
ping-core/            the client: stream, platforms, `ping`    │ the client
ping-agent/           AI agents: MCP server, runners           ─┘
pong/                 the host: sessions, pipelines, web UI    ─┐ the host
pong-app/             Pong's window and tray icon (GPUI)       ─┘
pingpong-*/           the libraries both share (see docs/architecture.md)
Casks/                the Homebrew casks, written at each release (see below)
docs/                 documentation; docs/design/ is the design history
site/                 the landing page (Astro), not part of the workspace (below)
tools/                build, deploy and test scripts (below)
spikes/               early experiments, not built with the workspace
vendor/               two no-op crates GPUI names
```

Most crates have an `examples/` directory of probes — small programs that
exercise one platform API and print what it does (a virtual display's mode,
what the NAT does, the pointer the host sees). Each says at its top what it
shows and how to run it.

## Building

Rust 1.96 or newer, and the platform packages in
[install.md](install.md#building-from-source).

```sh
cargo build                         # everything for this platform (debug)
cargo build --release -p ping-app   # one crate
```

- **Linux: build `pong` on its own**, not in one `cargo` invocation with the
  GPUI apps (a dependency is asked for two async runtimes; one build cannot
  have both). `cargo build --workspace --exclude pong` and
  `cargo build -p pong` are fine.
- **GPUI's Metal shaders** (macOS) compile at startup by default (the
  `runtime-shaders` feature), so no Xcode is needed; `--no-default-features`
  compiles them ahead of time with Xcode's Metal toolchain, as
  `tools/build-ping-app` does when Xcode is there.
- **Ping on Windows** needs FFmpeg and LLVM: use `tools/build-ping-win.ps1`
  (see [install.md](install.md#ping-on-windows)).

## Checking a change

Before sending a change, on your platform:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets     # no warnings
cargo test --workspace
```

and on the platforms you cannot run, at least type-check:

```sh
tools/xcheck-windows                              # cargo check for Windows, from a Mac
tools/xcheck-windows clippy --workspace --all-targets
tools/linux-dev cargo clippy --workspace --exclude pong --all-targets
tools/linux-dev cargo clippy -p pong --all-targets
```

`tools/xcheck-windows` uses a private rustup under
`~/.local/share/pingpong-xcheck` (it never touches your own toolchain) and
MinGW; the script's header has the one-time setup. The Windows client's
FFmpeg bindings need an FFmpeg 8 build's headers copied to
`~/.local/share/pingpong-xcheck/ffmpeg`. `tools/xcheck-linux` does the same
for crates that need no system libraries; everything else is checked in the
Linux container.

Tests that need hardware are `#[ignore]`d and say how to run them (for
example `pingpong-display/tests/windows_display.rs`, which must run in the
host's interactive desktop session).

### Linux

`tools/linux-dev CMD` runs a command in an Ubuntu 24.04 container with the
build dependencies (`tools/linux/Dockerfile`; on a Mac, Docker through
colima). The tree is mounted at `/src`; the build, the registry and git
checkouts live in named volumes, so rebuilds are incremental.

```sh
tools/linux-dev                                  # cargo build -p ping-app -p ping-core
tools/linux-dev cargo test -p pong
tools/linux-dev cargo test --workspace --exclude pong -j 1
tools/linux-dev tools/linux/loopback-test        # a Linux host streaming to a Linux client
```

- Link the GPUI apps one at a time (`-j 1` for tests): two at once can run
  the container out of memory.
- `tools/linux/loopback-test` runs Pong on one virtual X screen (a scene of
  moving boxes that logs the input it receives) and Ping on another, pairs
  them by hand, streams with scripted input and a tone, and leaves the logs,
  a picture and the input log in `target/linux-out/loopback`. With
  `PINGPONG_DOCKER_ARGS="-e LOOPBACK_PERMISSIONS=view,mouse"` the client is
  paired with those permissions, and the input log shows what the host
  let through (here the click and the wheel, no keys).
- `tools/linux/gnome-desktop` runs a GNOME desktop (mutter nested in an X
  screen, the portal, PipeWire) to test the Wayland host by hand;
  `pingpong-capture`'s `x11-poke` example answers its dialogs.
- `tools/linux/agent-desktop` is a small desktop (openbox, a terminal, an
  editor, a calculator, a file manager) with a Linux Pong, for AI agents to
  work on; `tools/linux/agent-tests/` has scripted clients for it.

### A Windows host from a Mac

- `tools/dev-sync [build ARGS]` mirrors the working tree to a Windows machine
  over SSH (tracked and untracked files, not ignored ones), and optionally
  runs `cargo build --release ARGS` there. Set `PINGPONG_SSH=user@host` and
  `PINGPONG_HOST_DIR` in the environment or in `tools/dev.env` (not
  committed).
- `tools/host-deploy.ps1` (on the host; it asks for an administrator when
  not run as one) installs the built `pong.exe` as `PongService`, and
  Pong's window.
- `tools/host-run.ps1 -Command '…'` runs a command in the interactive
  console session and prints its output: SSH lands in session 0, where there
  is no desktop to capture and no display to configure.
  `spikes/run-interactive.ps1` does the same for longer runs.
- `tools/set-resolution.ps1` sets the primary display's mode (for
  measurements on a host without a monitor); `tools/rawinput-probe` reports
  what raw input — the API games read — actually received.

### macOS signing

macOS ties the permissions it grants (Screen Recording, Accessibility, local
network) to an app's signature, and an ad hoc signature changes with every
build. `tools/dev-signing-identity` creates one self-signed code-signing
identity ("pingpong dev") in your login keychain; `tools/build-ping-app` and
`tools/build-pong-app` sign with it when it exists, so the permissions
survive rebuilds. The first signing asks to use the key: **Always Allow**.
`PINGPONG_SIGN_IDENTITY` names another identity to sign with, as a release
is ([Releases](#releases)).

## Test hooks

Environment variables that let a stream, or a window, be driven and impaired
without a person or a network tool. They are for tests; none is needed in
normal use.

| Variable | Does |
|---|---|
| `PING_TEST_INPUT="wait 3000; key 1e; click; text Hé!; quit"` | Scripted input once a stream is up (the steps are listed in `ping-core/src/bin/ping/script.rs`) |
| `PING_TEST_SNAPSHOT=PATH[@FRAMES]` | The Windows and Linux client save what is on screen as a PNG after that many frames |
| `PING_TEST_LOSS=N[:B]` | The client drops N% of incoming media datagrams, in bursts of B (after decryption) |
| `PINGPONG_TEST_WIRE_LOSS=N[:B]` | Any endpoint drops N% of all tunnel datagrams, in bursts of B (before decryption; handshakes included) |
| `PINGPONG_TEST_HANDSHAKE_LOSS=N` | Any endpoint drops N% of handshake datagrams (rekeys under loss) |
| `PINGPONG_TEST_BLACKOUT=S:D` | Any endpoint sends and receives nothing from S seconds after it opened, for D seconds |
| `PING_UI_DEMO=…`, `PONG_UI_DEMO=…` | Drive either window and save screenshots ([ui.md](ui.md#checking-it-without-clicking)) |
| `PING_UI_DEMO_SCREEN=PATH` | The picture the sample agent sessions show |
| `PONG_APP_URL`, `PONG_APP_CERT`, `PONG_APP_TOKEN` | Point Pong's window at another host's API |
| `PINGPONG_UPDATE_API=URL` | The update check asks this instead of `https://api.github.com` (the paths are GitHub's) |
| `PINGPONG_COMMIT=SHA`, `PINGPONG_RELEASE=1` | At build time: the commit the update check compares with `main` (else the checkout's), and a release build (follows releases only). `cargo run -p pingpong-update --example check [releases\|main]` asks GitHub as the apps would; `--example install -- ping\|pong FOLDER` installs the latest release into FOLDER as an update would (on a Mac, over an older release's apps there), leaving the installed apps alone |
| `PONG_NO_PROMPTS=1` | A bundled Mac host does not ask for permissions at start |
| `PONG_TEST_TAP_ONLY=PID[,PID]` | A Mac host's surround capture hears only these processes (a test signal's player): a client streaming the Mac to itself would otherwise send its own playback round again |
| `PING_AGENT_MCP="CMD … mcp"` | Agent runs start this MCP server instead of their own (e.g. one inside the Linux container) |
| `PING_AGENT_PATH_MAP=LOCAL=REMOTE` | Rewrites paths passed to such a server |
| `PING_AGENT_FREE_ONLY=1` | OpenRouter runs use free models only |
| `PINGPONG_PORTMAP_TEST=1` | Run the port-mapping test against this network's real router |

Per-second statistics: `RUST_LOG=info,ping_core::stats=debug` on the client
(`lost_frames` and `recoveries` in that line are cumulative), and the host's
`pong.log`.

## Releases

Every push to `main` that changes the programs is built and published:
`.github/workflows/release.yml` builds Ping and Pong on an Apple silicon
and an Intel Mac, on Windows and on Linux, and publishes the archives as
the website's downloads. Pushes that change only `docs/`, `Casks/` or
Markdown files build nothing. The website, [ping-pong.sh](https://ping-pong.sh),
is not on `main`: it lives on the `landing-page` branch, which is rebased
on `main` when it needs to be and never merged into it.

| Script | Makes |
|---|---|
| `tools/package-macos` | `Ping-VERSION-macos-ARCH.zip` (Ping.app) and `Pong-VERSION-macos-ARCH.zip` (Pong.app, Pong Control.app) |
| `tools/package-windows.ps1` | `Ping-VERSION-windows-x86_64.zip` (Ping.exe beside FFmpeg's DLLs) and `Pong-VERSION-windows-x86_64.zip` (pong.exe, Pong Control.exe, `install.ps1`, which is `tools/host-deploy.ps1`, and `Install Pong.cmd`) |
| `tools/linux/package` | `Ping-VERSION-linux-ARCH.tar.gz` and `Pong-VERSION-linux-ARCH.tar.gz`, each a folder with the programs, launcher entries, icons and `install.sh` (`tools/linux/install-release`) |
| `tools/publish-downloads DIST` | The downloads: each archive at `https://downloads.ping-pong.sh/latest/APP-OS-ARCH.EXT`, then `latest/SHA256SUMS` and `latest.json` (version, commit, sizes, checksums), in the Cloudflare R2 bucket `pingpong-downloads`, with wrangler |

All of them build with `PINGPONG_RELEASE=1`, so the programs follow
releases, not `main`. The downloads keep their names from one build to the
next, so the website links them as they are and reads `latest.json` for
what they are. The workflow signs in to Cloudflare with the repository's
secret `CLOUDFLARE_API_TOKEN` (an API token with **Workers R2 Storage:
Edit**) and variable `CLOUDFLARE_ACCOUNT_ID`; by hand, `wrangler login` is
enough.

A **version** is released by merging the change that sets it: the
workspace's version in `Cargo.toml` (`[workspace.package]`), which every
program has. When a push to `main` has a version with no `vVERSION` tag
yet, the same archives become the GitHub release `vVERSION`, and the
workflow opens a pull request that brings the Homebrew casks in `Casks/`
to it (`tools/update-casks`); `brew upgrade` sees the release once that is
merged. GitHub holds CI on a pull request a workflow opened until it is
approved: the workflow approves it, or says on the pull request that it
needs approving by hand.

To build the archives of a branch without publishing them, run the
workflow on it (`gh workflow run release.yml --ref BRANCH`): they are the
run's artifacts. To try the casks before a release, write them against the
local archives
(`CASKS_DIR=DIR tools/update-casks VERSION target/dist file://$PWD/target/dist`),
put them in a local tap (`brew tap-new`), and `brew install --cask
--appdir=DIR` from it.

On the runners the macOS apps are signed with a Developer ID, with the
hardened runtime and a secure timestamp, notarized by Apple and stapled,
so macOS opens them from a download. The workflow takes the certificate
and an App Store Connect API key from five secrets
(`MACOS_CERTIFICATE_P12`, the certificate and its key in base64;
`MACOS_CERTIFICATE_PASSWORD`; `APPLE_API_KEY_P8`, `APPLE_API_KEY_ID`,
`APPLE_API_ISSUER`), and a release on `main` fails without them. By hand,
with the Developer ID in your keychain:

```sh
PINGPONG_SIGN_IDENTITY="Developer ID Application: NAME (TEAMID)" \
APPLE_API_KEY_PATH=path/to/AuthKey_KEYID.p8 APPLE_API_KEY_ID=KEYID APPLE_API_ISSUER=ISSUER \
tools/package-macos
```

Without the API key the apps are signed but not notarized; without
`PINGPONG_SIGN_IDENTITY` they are signed as a development build is (below).

The Windows programs are not signed. Signing them takes a code-signing
certificate whose key stays with its issuer (every new one's must, in a
hardware module or the issuer's cloud): for a person rather than a
company, Certum's open-source certificate in its cloud is one, while
Microsoft's Artifact Signing issues to individuals only in the US and
Canada. The signing goes in `tools/package-windows.ps1`, on Ping.exe,
pong.exe and Pong Control.exe before they are zipped, with the issuer's
credentials as repository secrets, as the Developer ID's are.

`main` takes changes only through pull requests: work on a branch, push it,
and open one (`gh pr create`).

## Benchmarks and fuzzing

`cargo bench -p pingpong-proto`, `-p pingpong-transport` and
`-p pingpong-decode` run the Criterion benchmarks; the results and what the
end-to-end numbers are live in [benchmarks.md](benchmarks.md).

The protocol parsers that read network input are fuzzed (cargo-fuzz,
nightly Rust):

```sh
cd pingpong-proto/fuzz
cargo +nightly fuzz run depacketize   # also: control, input
```

## The website

`site/` is pingpong's landing page, built with [Astro](https://astro.build)
(Node 22.12 or newer). It reads the repository rather than copies of it
where it can: the UI's icons in `pingpong-ui/assets/icons/` and the
release in `Casks/ping.rb`, so a new release shows on its next build. Its
downloads are the newest build of `main` ([Releases](#releases)): the page
links their fixed names (`src/lib/downloads.ts`) and, as it loads, reads
`latest.json` for the version and the sizes. The page lives on the
`landing-page` branch only, rebased on `main` and never merged into it;
Cloudflare Pages builds [ping-pong.sh](https://ping-pong.sh) from there.

```sh
cd site
npm install
npm run dev       # http://localhost:4321, reloading as you edit
npm run build     # astro check, then the page in site/dist/
npm run preview   # serves site/dist/
```

`SITE_URL` and `BASE_PATH` say where the page is served from; for GitHub
Pages, `SITE_URL=https://mihaicristianfarcas.github.io BASE_PATH=/pingpong/
npm run build`. Without `SITE_URL` the page has no canonical URL.

What it shows is made from the apps, by scripts in `site/scripts/`:

| Script | Makes |
|---|---|
| `capture-apps.sh` (macOS) | The windows' screenshots in `src/assets/shots/`, from the demo mode in their dark appearance, with empty temporary data folders. Needs the release apps built and Screen Recording for the terminal. The hero's two windows (`src/assets/hero/`) are screenshots taken by hand. |
| `record-stream.sh` | The stream video in `public/media/`: a Linux host and client in the container (`tools/linux-dev`), the host showing a running clock (`clock-scene.py`), both screens read at the same instants from Xvfb's framebuffers (`grab-screens.py`) and set side by side; and the averages of Ping's statistics, for `src/lib/facts.ts`. A run whose slowest second fell below 55 fps (the VM starved, both sides at once) is refused and made again. |

`agent-screen.html` is the desktop the sample agent session is shown
(rendered as `agent-screen.webp`); `og-card.html` is the link preview
(`public/og.jpg`). Both are rendered at their size (1280x800, 1200x630) by
any headless browser. `src/assets/icons/` holds the apps' icons as SVG,
drawn as `ping-app/examples/make-icon.rs` draws them but with the ball and
its trail in blue; `public/favicon.svg` is Ping's.

Every figure on the page is one from the documentation or from that
recording, with the conditions it was measured under, and lives in
`src/lib/facts.ts` with where it comes from. When a figure there changes,
change it on the page too.

## Assets

- The icons are drawn by code: `cargo run -p ping-app --example make-icon
  -- OUTDIR` writes Ping's and Pong's macOS iconsets, their Windows `.ico`
  and 256-pixel PNGs, and Pong's menu bar mark (`tray.png`,
  `tray@2x.png`). The `.ico`, the PNGs and the mark are committed in
  `ping-app/assets/` and `pong-app/assets/` (`icon-256.png` there is
  `pong-256.png`); the bundle scripts rerun the drawing when it changes.
- The UI's icons are SVG files in `pingpong-ui/assets/icons/`, 24×24 with
  1.75 pt round strokes in `currentColor`, embedded in the binary.
- `pingpong-decode/tests/fixtures/testsrc.h264` is the known stream the
  decoder tests use (regenerate it with the FFmpeg command in
  `spikes/vt-latency/README.md`).
