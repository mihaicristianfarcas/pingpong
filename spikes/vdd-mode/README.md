# Spike: virtual display mode-setting on Windows 11 25H2

> Historical lab notebook (see [../README.md](../README.md)): `§` references
> are to [v2 design](../../docs/design/v2-design.md); "Task N" to the
> implementation plan of the time, which is in the git history only.

Answers spec **§6.3**, the risk the whole of §6 rests on, and gates Phase 1
(plan Task 1). Throwaway code; the deliverables are the findings below and the
IOCTL declarations in `src/main.rs` that Task 4 reuses.

Measured on the Windows test host: Windows 11 Pro, build **26200** (25H2), RTX 3070 Ti,
one physical monitor (AORUS AD27QD, 2560×1440 @ 144 Hz).

## Verdict

**Proceed. §6.3's risk does not apply, and the blocking scenario it feared is
real but avoidable.**

The premise of §6.1 was wrong: the host does not need
`VirtualDrivers/Virtual-Display-Driver` installed, because it already runs a
different VDD — **SudoVDA** (SudoMaker Virtual Display Adapter 1.10.9.289),
installed by **Apollo 0.4.6**. SudoVDA takes the mode as a *creation parameter*
over a private IOCTL, so there is no `ChangeDisplaySettingsEx` call to fail and
**issue #471 is moot**.

The one genuinely blocking question — can the virtual display be made primary? —
is **yes, but only via the CCD API**. `ChangeDisplaySettingsEx` cannot do it.
That is precisely the hypothesis §6.3 recorded as **[P]**; it is now **[V]**.

## Findings

### 1. [V] The mode is a creation parameter, and it is exact

`IOCTL_ADD_VIRTUAL_DISPLAY` with `{Width: 2560, Height: 1440, RefreshRate: 120000}`
produces a desktop-attached display at exactly **2560×1440 @ 120 Hz**, alongside
the physical 144 Hz panel, within ~1 s:

```
=== requesting 2560x1440 @ 120000 mHz ===
ADD ok: adapter_luid=0:112252 target_id=259
  \\.\DISPLAY1  primary=true   2560x1440 @ 144 Hz  [NVIDIA GeForce RTX 3070 Ti]
  \\.\DISPLAY5  primary=false  2560x1440 @ 120 Hz  [SudoMaker Virtual Display Adapter]
```

No mode-change call is involved, so the failure mode §6.3 feared cannot occur.

**[V] `RefreshRate` accepts millihertz.** `Driver.cpp` does
`if (VSync < 1000) VSync *= 1000`, so the field takes Hz *or* millihertz. Our
wire protocol's `refresh_mhz` (§4.4) passes straight through with no conversion,
which independently validates that millihertz choice.

### 2. [V] `ChangeDisplaySettingsEx` fails **only** for `CDS_SET_PRIMARY`

A plain mode change on the virtual display works:

```
=== forcing mode on \\.\DISPLAY5 (currently 2560x1440@120) ===
ChangeDisplaySettingsExW(mode) returned 0 (0 = success)
VERIFIED: mode forced to 1920x1080@60 after arrival
```

So issue #471's *symptom* — a mode change reporting success without moving the
desktop mode — does not reproduce here at all. Only the set-primary flag is
rejected, which is finding 2a.

### 2a. [V] `CDS_SET_PRIMARY` FAILS on the virtual display

```
staged rcs=[0, -1]  apply rc=0
  \\.\DISPLAY1 primary=true  at (0,0)
  \\.\DISPLAY5 primary=false at (2560,0)
```

`-1` is `DISP_CHANGE_FAILED`, returned for the VDD's own `CDS_SET_PRIMARY`
staging call. The physical display's staging call returns `0`, so this is
specific to the indirect display, not to the call sequence.

This was measured with the *correct* multi-display sequence — translate every
display by the target's offset, stage each with `CDS_NORESET`, then commit with
one `ChangeDisplaySettingsExW(NULL, ...)`. The naive version (reposition only
the target) returns `DISP_CHANGE_SUCCESSFUL` and silently does nothing, which
looks identical to a driver limitation. Worth knowing before re-deriving it.

### 3. [V] `SetDisplayConfig` CAN make it primary — this is the answer to §6.3's [P]

```
ChangeDisplaySettingsEx could not do it; trying the CCD path
VERIFIED: the virtual display CAN be made primary, via SetDisplayConfig
  \\.\DISPLAY1 primary=false at (-2560,0)
  \\.\DISPLAY5 primary=true  at (0,0)
```

`QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS)` → translate every
`DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE` position by the target's offset →
`SetDisplayConfig(SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES
| SDC_SAVE_TO_DATABASE)`.

The virtual display is located by the `adapterLuid` + `targetId` that
`IOCTL_ADD_VIRTUAL_DISPLAY` returns, matched against `path.targetInfo`. No
name-matching or "last enumerated display" heuristic is needed — the driver
hands us an unambiguous identity.

**Consequence: §6.3's blocking scenario does not occur.** Games launch on the
primary display, and the virtual display can be primary.

### 4. [V] The watchdog is real, and a keepalive is MANDATORY

SudoVDA runs a global 3-second watchdog. Every IOCTL *except* `IOCTL_GET_WATCHDOG`
resets the countdown; at zero the driver calls `DisconnectAllMonitors()`.

This was initially invisible — the countdown sat pinned at `3` for a full 8 s
and nothing was reaped:

```
t=0.5s countdown=3 display_alive=true
...
t=8.0s countdown=3 display_alive=true
```

The cause was **ApolloService**, which polls the driver roughly once a second and
resets the shared countdown. With Apollo stopped, the real behaviour appears
immediately:

```
t=0.5s countdown=2 display_alive=true
t=1.0s countdown=1 display_alive=true
t=1.5s countdown=1 display_alive=true
t=2.0s countdown=0 display_alive=false
VERIFIED: watchdog reaped the display after 2s without a ping
```

`pingpong-server` **must** run its own keepalive thread pinging at ≤1 s. Relying
on Apollo being installed and running would be a silent dependency that fails
the moment the user stops it — and the failure mode is the streamed display
vanishing mid-session.

### 5. [V] Teardown is clean, and post-reap REMOVE must be tolerated

`IOCTL_REMOVE_VIRTUAL_DISPLAY` returns the display list to its original state
(`teardown clean: true`). If the watchdog already reaped the monitor, REMOVE
returns `0x80070490 ERROR_NOT_FOUND`. Task 4 must treat that as success, not as
an error — it means the desired end state already holds.

**[V] ADD is idempotent by `MonitorGuid`.** `Driver.cpp` returns the existing
monitor's `adapterLuid`/`targetId` with `STATUS_SUCCESS` when the GUID matches,
without recreating it. This gives `DisplayControl::activate`'s required
idempotency (§4.4) for free — but note it returns the existing monitor **even if
the requested mode differs**, so a mode *change* requires REMOVE-then-ADD.

Using a fixed GUID (rather than a fresh one per run) also stops repeated runs
from leaking connector slots; SudoVDA returns `STATUS_TOO_MANY_NODES` once they
are exhausted.

### 6. [V] Display work must run in the interactive session

An SSH shell lands in session 0, whose desktop reports a synthetic
`WinDisc 1024x768` and never sees the real monitors. This is the same constraint
`run-interactive.ps1` documents for WGC, but it applies to **display enumeration
and mode-setting too**, which the plan did not state. Everything here was run via
`run-interactive.ps1`.

### 7. [V] Windows OVERRIDES the requested mode with a saved per-monitor config

The mode passed to ADD is not what you necessarily get. Windows persists a
display configuration keyed on the monitor's EDID identity and reapplies it when
that monitor reappears:

```
=== ADD 1920x1080 @ 60000 mHz ===
ADD ok: luid=0:112252 target_id=263
  t=  0.25s  \\.\DISPLAY1:2560x1440@59*  \\.\DISPLAY5:2560x1440@120
```

A request for 1920×1080 @ 60 produced 2560×1440 @ 120 — the mode a *previous*
session had left saved for this monitor — and it came up already primary. Using
`SDC_SAVE_TO_DATABASE` when setting primary strengthens that persistence.

**Consequence:** `activate()` must force the mode with `ChangeDisplaySettingsEx`
*after* the monitor attaches, and verify by read-back. Finding 1's "the mode is
exact" holds only for a monitor identity Windows has never seen before, which is
true exactly once.

### 8. [V] Arrival and departure are asynchronous; detect by CCD identity

Removal returns as soon as the driver has requested departure — the display is
still attached for a while afterwards. Detecting our monitor by diffing attached
GDI names against a "before" snapshot therefore breaks on a mode change: the
outgoing display is still in the snapshot, so no "new" name ever appears and the
wait times out. This cost a full debugging round.

Use the `adapterLuid` + `targetId` from `AddOut` instead, resolved to a GDI name
via `QueryDisplayConfig` + `DisplayConfigGetDeviceInfo(GET_SOURCE_NAME)`. That
is unambiguous, survives a stale monitor lingering from an earlier run, and
gives a departure test for free. `target_id` increments per ADD, so a fresh
identity cannot be confused with the outgoing one.

## Spec consequences

- **§6.1 is superseded.** The driver is SudoVDA (already installed via Apollo),
  not `VirtualDrivers/Virtual-Display-Driver`. Its issue #471 is irrelevant.
- **§6.3's [P] becomes [V]:** the CCD API is required, but for *setting primary*
  rather than for committing a mode change.
- **§6.1's "mode is a creation parameter" needs the caveat in finding 7:** it is
  true only for a monitor Windows has not seen before. In steady state the host
  must force the mode after arrival.
- **§6.4 gains a second failure mode:** the watchdog. Crash-restore must also
  account for the display disappearing while the process is alive but wedged.
- **§4.4's millihertz choice is validated** by the driver's own unit handling.

## Reproducing

Must run in the interactive desktop session:

```bash
COPYFILE_DISABLE=1 tar czf - spikes/vdd-mode | ssh user@windows-host 'cd C:\path\to\pingpong; tar xzf -'
ssh user@windows-host 'cd C:\path\to\pingpong\spikes\vdd-mode; cargo build --release'
ssh user@windows-host 'powershell -ExecutionPolicy Bypass -File C:\path\to\pingpong\spikes\run-interactive.ps1 -WorkDir C:\path\to\pingpong\spikes\vdd-mode -Exe .\target\release\vdd-mode.exe -TimeoutSec 150'
```

Optional args: `width height refresh_mhz` (default `2560 1440 120000`).

To observe the watchdog rather than Apollo's keepalive, stop the service first
and **restart it afterwards**:

```bash
ssh user@windows-host 'Stop-Service ApolloService'
# ... run ...
ssh user@windows-host 'Start-Service ApolloService'
```

## Contract source

`SudoMaker/SudoVDA` — `Common/Include/sudovda-ioctl.h` and
`Virtual Display Driver (HDR)/SudoVDA/Driver.cpp`, cross-checked against the
installed driver (`oem35.inf`, 1.10.9.289, protocol 0.2.1). Interface GUID
`{e5bcc234-1e0c-418a-a0d4-ef8b7501414d}`; IOCTLs are
`CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800..0x8FF, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
