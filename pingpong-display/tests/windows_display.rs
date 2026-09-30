//! Hardware tests: need SudoVDA (installed via Apollo) and a real desktop.
//!
//! MUST run in the interactive session -- session 0 sees a synthetic
//! `WinDisc 1024x768` and no real monitors, so every assertion here would be
//! meaningless there. Over SSH, from the repository on the host:
//!
//!   spikes\run-interactive.ps1 -WorkDir C:\path\to\pingpong -Exe cargo.exe `
//!     -Arguments "test -p pingpong-display --test windows_display -- --ignored --test-threads=1"
//!
//! `--test-threads=1` is required: these all drive the one physical desktop.
#![cfg(windows)]

use pingpong_display::windows::{virtual_display_present, WindowsDisplay};
use pingpong_display::{DisplayControl, DisplayMode};

fn state_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("pingpong-display-test-state-{tag}.toml"));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
#[ignore = "requires the virtual display driver"]
fn activate_then_restore_returns_the_desktop_to_where_it_started() {
    let mut d = WindowsDisplay::new(state_path("activate"));
    let mode = DisplayMode {
        width: 1920,
        height: 1080,
        refresh_mhz: 60_000,
    };

    let active = d.activate(mode).expect("activates");
    assert_eq!(active.mode.width, 1920);
    assert_eq!(active.mode.height, 1080);
    assert!(virtual_display_present(), "no virtual display attached");

    d.restore().expect("restores");
    assert!(
        !virtual_display_present(),
        "restore left the virtual display attached"
    );
    // Restoring twice must not fail -- teardown can race a crash handler.
    d.restore().expect("second restore is a no-op");
}

#[test]
#[ignore = "requires the virtual display driver"]
fn activating_an_already_active_mode_is_a_no_op() {
    // A duplicated SessionStart must cost nothing (v2 design §4.4).
    let mut d = WindowsDisplay::new(state_path("idempotent"));
    let mode = DisplayMode {
        width: 1920,
        height: 1080,
        refresh_mhz: 60_000,
    };
    let first = d.activate(mode).expect("activates");
    let second = d.activate(mode).expect("activates again");
    assert_eq!(first, second);
    d.restore().expect("restores");
}

#[test]
#[ignore = "requires the virtual display driver"]
fn the_display_survives_longer_than_the_watchdog() {
    // The driver reaps every monitor ~3 s after the last ioctl (v2 design §6.1).
    // This is the test that the keepalive thread actually runs -- without it a
    // session dies about three seconds in, which no unit test can catch.
    let mut d = WindowsDisplay::new(state_path("watchdog"));
    let mode = DisplayMode {
        width: 1920,
        height: 1080,
        refresh_mhz: 60_000,
    };
    d.activate(mode).expect("activates");

    std::thread::sleep(std::time::Duration::from_secs(8));

    assert!(
        virtual_display_present(),
        "watchdog reaped the display -- the keepalive thread is not running"
    );
    d.restore().expect("restores");
}

#[test]
#[ignore = "requires the virtual display driver"]
fn a_different_mode_replaces_the_active_one() {
    // ADD is idempotent by monitor GUID and returns the OLD monitor when the
    // mode differs, so activate() must REMOVE first or silently serve a stale
    // mode. Distinct refresh rates as well as sizes, so a partial fix shows up.
    let mut d = WindowsDisplay::new(state_path("modechange"));
    let a = d
        .activate(DisplayMode {
            width: 1920,
            height: 1080,
            refresh_mhz: 60_000,
        })
        .expect("activates");
    let b = d
        .activate(DisplayMode {
            width: 1280,
            height: 720,
            refresh_mhz: 120_000,
        })
        .expect("re-activates");

    assert_eq!(a.mode.width, 1920);
    assert_eq!(
        b.mode.width, 1280,
        "stale mode served -- REMOVE-then-ADD is missing"
    );
    assert!(virtual_display_present());
    d.restore().expect("restores");
}

#[test]
#[ignore = "requires the virtual display driver"]
fn restore_puts_the_original_primary_back() {
    // v2 design §6.4: the session moves the primary to the virtual display, so
    // teardown must move it back or the user is left on the wrong monitor.
    let before = pingpong_display::windows::primary_id().expect("a primary exists");

    let mut d = WindowsDisplay::new(state_path("primary"));
    d.activate(DisplayMode {
        width: 1920,
        height: 1080,
        refresh_mhz: 60_000,
    })
    .expect("activates");

    let during = pingpong_display::windows::primary_id().expect("a primary exists");
    assert_ne!(during, before, "the virtual display did not become primary");

    d.restore().expect("restores");
    let after = pingpong_display::windows::primary_id().expect("a primary exists");
    assert_eq!(
        after, before,
        "restore did not put the original primary back"
    );
}
