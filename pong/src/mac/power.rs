//! Keep a Mac streaming: its display must be awake to be captured at all
//! (ScreenCaptureKit offers no display while it sleeps), and a remote user
//! resets no idle timer. As `caffeinate -u` does: declare user activity to
//! wake the display, and hold an assertion against display sleep for the
//! session.

use std::ffi::c_void;

use objc2_core_foundation::CFString;

type IOPMAssertionID = u32;
const K_IOPM_USER_ACTIVE_LOCAL: u32 = 0;
const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionDeclareUserActivity(
        name: *const c_void,
        user_type: u32,
        id: *mut IOPMAssertionID,
    ) -> i32;
    fn IOPMAssertionCreateWithName(
        kind: *const c_void,
        level: u32,
        name: *const c_void,
        id: *mut IOPMAssertionID,
    ) -> i32;
    fn IOPMAssertionRelease(id: IOPMAssertionID) -> i32;
}

/// Held for a session: the display stays on until it is dropped.
pub struct Awake {
    activity: IOPMAssertionID,
    no_sleep: IOPMAssertionID,
}

impl Awake {
    /// Wake the display (as a key press would) and keep it awake.
    pub fn hold() -> Awake {
        let name = CFString::from_str("Pong is streaming this Mac");
        let kind = CFString::from_str("PreventUserIdleDisplaySleep");
        let (mut activity, mut no_sleep) = (0, 0);
        unsafe {
            let rc = IOPMAssertionDeclareUserActivity(
                cf(&name),
                K_IOPM_USER_ACTIVE_LOCAL,
                &mut activity,
            );
            if rc != 0 {
                tracing::warn!(rc, "could not wake the display");
            }
            let rc = IOPMAssertionCreateWithName(
                cf(&kind),
                K_IOPM_ASSERTION_LEVEL_ON,
                cf(&name),
                &mut no_sleep,
            );
            if rc != 0 {
                tracing::warn!(rc, "could not keep the display awake");
            }
        }
        Awake { activity, no_sleep }
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        unsafe {
            for id in [self.no_sleep, self.activity] {
                if id != 0 {
                    IOPMAssertionRelease(id);
                }
            }
        }
    }
}

fn cf(s: &CFString) -> *const c_void {
    s as *const CFString as *const c_void
}
