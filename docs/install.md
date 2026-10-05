# Installing

pingpong has two programs: **Ping**, the client, on the computer you sit at,
and **Pong**, the host, on the computer you stream. Every system has them
ready to [download](#from-the-website) from
[ping-pong.sh](https://ping-pong.sh/#install); on a Mac they also install
with [Homebrew](#with-homebrew-macos), and everywhere they build from this
repository with Rust. Pick the sections for your systems:

| | macOS 14+ | Windows 10/11 | Linux |
|---|---|---|---|
| Ping (client) | [Download](#on-macos), [Homebrew](#with-homebrew-macos) or [Ping.app](#ping-on-macos) | [Download](#on-windows) or [Ping.exe](#ping-on-windows) | [Download](#on-linux) or [ping-app](#ping-on-linux) |
| Pong (host) | [Download](#on-macos), [Homebrew](#with-homebrew-macos) or [Pong.app](#pong-on-macos) | [Download](#on-windows) or [PongService](#pong-on-windows) | [Download](#on-linux) or [a user service](#pong-on-linux) |

Then [pair them](#pairing).

## From the website

[ping-pong.sh](https://ping-pong.sh/#install) asks which computer you are
installing on and what it runs, and gives you its download: the newest
build of the `main` branch, made by the release workflow
([development.md](development.md#releases)). The files keep their
addresses from one build to the next, for scripts, under
`https://downloads.ping-pong.sh/latest/`:

| | macOS, Apple silicon | macOS, Intel | Windows, x86_64 | Linux, x86_64 |
|---|---|---|---|---|
| Ping | `Ping-macos-arm64.zip` | `Ping-macos-x86_64.zip` | `Ping-windows-x86_64.zip` | `Ping-linux-x86_64.tar.gz` |
| Pong | `Pong-macos-arm64.zip` | `Pong-macos-x86_64.zip` | `Pong-windows-x86_64.zip` | `Pong-linux-x86_64.tar.gz` |

`latest/SHA256SUMS` has their checksums, and `latest.json` says which
version and commit they are.

### On macOS

Open the zip and move the apps to Applications: Ping, or Pong and Pong
Control. They are signed with a Developer ID and notarized by Apple, so
macOS opens them as any app from the internet, asking once whether you
want to. Then, for the host, open **Pong Control** and choose **Start
Pong**, as in [Pong on macOS](#pong-on-macos).

### On Windows

The programs are not code-signed yet, so Windows asks before it first runs
each one, and names its publisher as unknown.

- **Ping**: extract the zip into a folder of its own (Ping.exe needs
  FFmpeg's DLLs beside it) and open **Ping.exe**. SmartScreen asks first:
  **More info**, then **Run anyway**.
- **Pong**: the host needs what [Pong on Windows](#pong-on-windows) lists
  first (an NVIDIA GPU, SudoVDA). Extract the zip and double-click
  **Install Pong.cmd**. Windows asks whether to run it, then whether
  Windows PowerShell may make changes as an administrator. It does what
  `host-deploy.ps1` does there (PongService, the Start menu entry, the
  icon at sign-in). Run it from a newer download to update.

SmartScreen judges an unsigned program by the file itself, so it may ask
again after an update: each build is a new file. On a PC where **Smart
App Control** is on (some new installations of Windows 11), the programs
do not run at all: it blocks unsigned programs it does not know, with no
way to allow one. They run there only with Smart App Control turned off
(Windows Security > App & browser control > Smart App Control settings),
which is for whoever owns the PC to decide.

### On Linux

The programs are built on Ubuntu 24.04 and use the system's libraries as
it has them (FFmpeg 6, PipeWire): they run on Ubuntu 24.04 and the
distributions built on it. Elsewhere, [build from source](#building-from-source).

```sh
tar xzf Ping-VERSION-linux-x86_64.tar.gz
Ping-VERSION-linux-x86_64/install.sh
```

`install.sh` puts the programs in `~/.local/bin`, each with a launcher
entry; Pong's also installs the host as a user service and starts it (see
[Pong on Linux](#pong-on-linux)). It names any library the system is
missing. `install.sh --uninstall` removes what it installed.

## With Homebrew (macOS)

This repository is a Homebrew tap: its casks install the apps from the
latest release (Apple silicon and Intel).

```sh
brew tap mihaicristianfarcas/pingpong https://github.com/mihaicristianfarcas/pingpong
brew install --cask ping          # the client
brew install --cask pong          # the host: Pong.app and Pong Control.app
```

For the host, open **Pong Control** and choose **Start Pong**: the host
then runs whenever you are logged in, and Pong's icon is in the menu bar.
macOS asks for Screen Recording and Accessibility the first time (see
[Pong on macOS](#pong-on-macos)).

`brew upgrade --cask ping pong` updates them; the apps say when there is an
update ([usage.md](usage.md#updates)). The apps are signed with a
Developer ID and notarized by Apple.

## Building from source

Every system needs **Rust 1.96 or newer** (`rustup` from
[rustup.rs](https://rustup.rs) is the easy way) and git. The first build
downloads and compiles GPUI (Zed's UI framework) and a few hundred other
crates; expect several minutes.

```sh
git clone https://github.com/mihaicristianfarcas/pingpong
cd pingpong
```

What else each system needs:

- **macOS**: the Xcode Command Line Tools (`xcode-select --install`). Full
  Xcode is optional: with its Metal toolchain the app bundles carry their
  shaders compiled.
- **Windows**: the MSVC toolchain (Visual Studio Build Tools with "Desktop
  development with C++") and the Rust `x86_64-pc-windows-msvc` target, which
  `rustup` installs by default. The client also needs FFmpeg and LLVM (see
  [Ping on Windows](#ping-on-windows)).
- **Linux** (Ubuntu 24.04 package names; other distributions have
  equivalents):

  ```sh
  sudo apt install build-essential pkg-config clang libclang-dev \
      libavcodec-dev libavutil-dev libswscale-dev libva-dev \
      libasound2-dev libpulse-dev libudev-dev \
      libx11-dev libx11-xcb-dev libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
      libxcursor-dev libxrandr-dev libxi-dev libfontconfig-dev libfreetype-dev libxcb1-dev \
      libpipewire-0.3-dev libspa-0.2-dev libdbus-1-dev
  ```

  (`tools/linux/Dockerfile` is the authoritative list; it is what the
  project is built and tested with.)

## Ping on macOS

```sh
tools/build-ping-app              # builds target/Ping.app
tools/build-ping-app --install    # and copies it to /Applications
```

The first time Ping starts, macOS asks whether it may find devices on the
local network: allow it, or Ping cannot see or reach hosts on your LAN.

The build is signed ad hoc, or with a local development identity if you
created one (`tools/dev-signing-identity`, see
[development.md](development.md#macos-signing)).

## Pong on macOS

```sh
tools/build-pong-app --install
```

This installs **Pong.app** (the host) and **Pong Control.app** (its window,
and Pong's menu bar icon) in `/Applications`, and two LaunchAgents: one
starts the host at login and keeps it running
(`~/Library/LaunchAgents/dev.pingpong.Pong.plist`), the other puts Pong's
icon in the menu bar at login (`dev.pingpong.PongControl.plist`; **Show
Pong's icon at login** in the window turns it off). On first start Pong
asks for **Screen Recording** (to capture) and **Accessibility** (to use
the keyboard and mouse); grant both in System Settings > Privacy &
Security. Without Accessibility, clients can watch but not control.

Without `--install`, the apps are left in `target/`. To run the host from a
terminal instead: `cargo run --release -p pong -- host` (the terminal then
needs those permissions). Details and limits: [platforms/macos.md](platforms/macos.md).

## Pong on Windows

First, on the host PC:

1. An **NVIDIA GPU** and a current driver (Pong encodes with NVENC).
2. **SudoVDA**, the virtual display driver: installed by
   [Apollo](https://github.com/ClassicOldSong/Apollo), or from
   [SudoMaker/SudoVDA](https://github.com/SudoMaker/SudoVDA). If Apollo is
   installed, stop its service while Pong runs (they share the driver).
3. Optional: **[ViGEmBus](https://github.com/nefarius/ViGEmBus)**, for
   controllers; **Steam** (its *Steam Streaming Speakers* device keeps the
   sound on the client only, as Sunshine does).

Then, from the repository, in an **administrator** PowerShell:

```powershell
cargo build --release -p pong -p pong-app
powershell -ExecutionPolicy Bypass -File tools\host-deploy.ps1
```

`host-deploy.ps1` copies the host to `C:\Program Files\Pong`, installs and
starts **PongService** (it starts at boot, before anyone signs in, and adds
Windows Firewall rules for Pong), puts Pong's window in the Start menu as
**Pong**, and puts Pong's icon in the taskbar's notification area from your
next sign-in on (your `Run` key; **Show Pong's icon at login** in the window
turns it off). Run it again after each build to update. `pong uninstall` (as
administrator, from `C:\Program Files\Pong`) removes the service and its
firewall rules.

Pong's settings and state are in `C:\ProgramData\Pong`. Details:
[platforms/windows.md](platforms/windows.md).

## Ping on Windows

Ping's Windows build decodes with FFmpeg, so it needs:

- an **FFmpeg 8 shared build** with `include\`, `lib\` and `bin\` — for
  example BtbN's `ffmpeg-n8.1-latest-win64-lgpl-shared` from
  [github.com/BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases);
- **LLVM** (for `libclang`, which generates the FFmpeg bindings), in
  `C:\Program Files\LLVM` or wherever `LIBCLANG_PATH` points.

```powershell
powershell -ExecutionPolicy Bypass -File tools\build-ping-win.ps1 -FFmpeg C:\path\to\ffmpeg
powershell -ExecutionPolicy Bypass -File tools\build-ping-win.ps1 -FFmpeg C:\path\to\ffmpeg -Install
```

(`-FFmpeg` can be left out when `FFMPEG_DIR` is set.) The first leaves
`target\Ping\Ping.exe` beside FFmpeg's DLLs; `-Install` copies them to
`%LOCALAPPDATA%\Programs\Ping` and adds **Ping** to the Start menu. Windows
Firewall may ask to let Ping on the network the first time: allow private
networks.

## Ping on Linux

```sh
cargo build --release -p ping-app          # the app: target/release/ping-app
cargo build --release -p ping-core --bin ping   # the CLI, if you want it
```

Run `target/release/ping-app`, or install it as a desktop app for your
user, with a launcher entry and its icon:

```sh
tools/linux/install-apps          # ping-app and pong-app, whichever are built
```

Details: [platforms/linux.md](platforms/linux.md).

## Pong on Linux

Build the host **on its own** — not in one `cargo` invocation with the GPUI
apps: GPUI and Pong's desktop-portal code ask one dependency for different
async runtimes, and one build cannot have both.

```sh
cargo build --release -p pong
install -Dm755 target/release/pong ~/.local/bin/pong
install -Dm644 tools/linux/pong.service ~/.config/systemd/user/pong.service
systemctl --user enable --now pong
```

Pong runs in your desktop session, as you. Under Wayland, the desktop asks
once whether Pong may share the screen (on GNOME, switch on **Allow Remote
Interaction** for the keyboard and mouse). Pong's window is
`cargo build --release -p pong-app`, then `target/release/pong-app` (or
`tools/linux/install-apps` for a launcher entry). Data and logs are in
`~/.config/pong`. Details: [platforms/linux.md](platforms/linux.md).

## Pairing

1. Open Ping. Hosts on your local network appear on the **Hosts** page;
   others can be added by address (**Add Host**, or ⌘N / Ctrl+N).
2. Click the host: Ping shows a four-digit PIN.
3. On the host, open Pong's window (**Devices**) or its web UI
   (`https://HOST:47802`, whose certificate is self-signed) and type the
   PIN.

The first time you open the web UI it asks you to create its admin account.
Pairing is done once, on the local network; afterwards Ping connects from
anywhere ([networking.md](networking.md)). Next: [usage.md](usage.md).

## Uninstalling

| | |
|---|---|
| Homebrew | `brew uninstall --cask ping pong` (the host's LaunchAgents go too); `--zap` also deletes the data folders |
| Ping, macOS | Delete `/Applications/Ping.app` and `~/Library/Application Support/Ping` |
| Pong, macOS | `launchctl bootout gui/$(id -u) ~/Library/LaunchAgents/dev.pingpong.Pong.plist` and the same for `dev.pingpong.PongControl.plist`, delete those files, the two apps, and `~/Library/Application Support/Pong` |
| Ping, Windows | Delete `%LOCALAPPDATA%\Programs\Ping`, its Start menu entry and `%APPDATA%\Ping` |
| Pong, Windows | `pong uninstall` as administrator, then delete `C:\Program Files\Pong`, `C:\ProgramData\Pong`, `%APPDATA%\Pong`, the Start menu entry, and the `Pong` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` |
| Ping, Linux | `install.sh --uninstall` from the download, or `tools/linux/install-apps --uninstall` (or delete the binary); and `~/.config/ping` |
| Pong, Linux | `install.sh --uninstall` from the download; or `systemctl --user disable --now pong`, then delete the unit and the binary. Then `~/.config/pong` |

Unpair a device on the other side too (Ping: the host's menu > Unpair; Pong:
**Devices**).
