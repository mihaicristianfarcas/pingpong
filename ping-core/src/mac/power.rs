//! Low Power Mode, watched while a stream runs: on a MacBook Pro it holds
//! the 120 Hz ProMotion panel at 60 Hz (NSScreen still says 120), and at
//! 60 Hz a frame reaches the glass two refreshes after it is committed,
//! even straight to the display (Metal's HUD says "Direct").
//!
//! Measured with `examples/mac-present` on an M4 Pro, a 60 fps 1920x1080
//! picture full screen: 33-39 ms from decode to the glass with Low Power
//! Mode on, 6 ms with it off; 3.6 ms on with V-Sync off (which tears).
//! Nothing Ping can ask of the display shortens it: Game Mode forced on
//! measured 34 ms, presenting at a time or after a minimum duration
//! (`presentDrawable:atTime:`, `afterMinimumDuration:`) 33-46 ms, and a
//! `CAMetalDisplayLink` at 60 fps with a frame's latency 42 ms. So while
//! Low Power Mode lasts, V-Sync is held off (unless frame pacing was
//! chosen, which asks for evenness tearing would undo), and the person is
//! told in the corner where connection warnings go; the log says when it
//! changes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use objc2_foundation::NSProcessInfo;

use super::render::RenderShared;

/// How often the setting is looked at: someone who plugs in, or turns it
/// off, sees the warning go within a second.
const LOOK_EVERY: Duration = Duration::from_secs(1);

/// V-Sync held off.
const VSYNC_OFF: &str = "Low Power Mode: V-Sync off, so the picture may tear";
/// Frame pacing kept.
const LATE: &str = "Low Power Mode is on: the picture is ~30 ms late";

/// Panels faster than this are the ones Low Power Mode slows (a 60 Hz
/// display is at its rate already).
const FAST_PANEL_HZ: f64 = 100.0;

/// Watches Low Power Mode until dropped.
pub(crate) struct PowerWatch {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PowerWatch {
    /// `refresh`: the display's shortest refresh interval, in seconds (0:
    /// not known); `warn`: say so in the stream (the connection warnings
    /// setting).
    pub(crate) fn start(render: Arc<RenderShared>, refresh: f64, warn: bool) -> PowerWatch {
        let fast_panel = refresh > 0.0 && 1.0 / refresh > FAST_PANEL_HZ;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("ping-power".into())
                .spawn(move || watch(&render, fast_panel, warn, &stop))
                .ok()
        };
        PowerWatch { stop, thread }
    }
}

impl Drop for PowerWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn watch(render: &RenderShared, fast_panel: bool, warn: bool, stop: &AtomicBool) {
    let mut was: Option<bool> = None;
    while !stop.load(Ordering::Relaxed) {
        let on = NSProcessInfo::processInfo().isLowPowerModeEnabled();
        if was != Some(on) {
            if on || was.is_some() {
                tracing::info!(on, "Low Power Mode");
            }
            // With V-Sync off already, nothing waits for a refresh: no cost.
            if fast_panel && render.waits_for_refresh() {
                let note = if render.hold_vsync_off(on) {
                    VSYNC_OFF
                } else {
                    LATE
                };
                // Cleared only if it was this note that was shown.
                if warn && (on || was == Some(true)) {
                    render.set_warning(on.then(|| note.to_string()));
                }
            }
            was = Some(on);
        }
        // In short steps, so a stream closes without waiting a second.
        for _ in 0..10 {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(LOOK_EVERY / 10);
        }
    }
}
